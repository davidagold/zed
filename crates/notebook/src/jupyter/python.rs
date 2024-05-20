use std::sync::OnceLock;

use gpui::{AsyncAppContext, Task};
use log::{error, info};
use pyo3::exceptions::PyRuntimeError;
use pyo3::types::{PyAnyMethods, PyDict, PyString, PyTuple};
use pyo3::{pyclass, pymethods, Bound, IntoPy, Py, PyAny, PyResult, Python};
use serde::Deserialize;
use serde::{self, de::Error};
use tokio::sync::oneshot;

use crate::do_in;

#[macro_export]
macro_rules! kwargs {
    ($py:ident, { $( $key:literal => $val:expr $(,)? )* }) => {
        {
            (|py| -> PyResult<Bound<'_, PyDict>> {
                fn __kwargs<'py>(py: Python<'py>) -> Bound<'py, PyDict> {
                    PyDict::new_bound(py)
                }
                let kwargs = __kwargs(py);
                $(
                    let key = PyString::new_bound(py, $key);
                    kwargs.set_item(key, &$val)?;
                )*

                Ok(kwargs)
            })($py)
        }
    }
}

pub(crate) struct Pydantic<'py>(pub &'py Bound<'py, PyAny>);

impl<'a> Pydantic<'a> {
    pub(crate) fn deserialize<M: for<'de> Deserialize<'de>>(&self) -> Result<M, serde_json::Error> {
        let json = match do_in!(|py| {
            let kwargs = &kwargs!(py, { "exclude_none" => true })?;
            self.0
                .call_method("model_dump_json", (), Some(&kwargs))
                .map(|obj| obj.to_string())
        }) {
            Ok(json) => json,
            Err(err) => {
                do_in!(|py| err.print(py));
                return Err(serde_json::Error::custom("Failed to dump model"));
            }
        };
        serde_json::from_str::<M>(json.as_str())
    }
}

pub(crate) trait TryAsStr {
    fn __str__(&self) -> Option<String>;
}

impl TryAsStr for Py<PyAny> {
    fn __str__(&self) -> Option<String> {
        do_in!(|py| -> Option<String> {
            let method = self.getattr(py, PyString::new_bound(py, "__str__")).ok()?;
            let result = method.call_bound(py, (), None).ok()?;
            let py_string = result.downcast_bound::<PyString>(py).ok()?;
            py_string.extract::<String>().ok()
        })
    }
}

impl TryAsStr for Bound<'_, PyAny> {
    fn __str__(&self) -> Option<String> {
        self.clone().unbind().__str__()
    }
}

pub(crate) fn get_running_loop<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    py.import_bound("asyncio")?
        .getattr("get_running_loop")?
        .call0()
}

pub(crate) fn ensure_future<'py>(
    py: Python<'py>,
    coro: &Py<PyAny>,
    event_loop: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    py.import_bound("asyncio")?.getattr("ensure_future")?.call(
        (coro.bind(py),),
        event_loop
            .and_then(|event_loop| kwargs!(py, { "loop" => event_loop }).ok())
            .as_ref(),
    )
}

#[pyclass]
pub(crate) struct Callback {
    tx: OnceLock<oneshot::Sender<PyResult<Py<PyAny>>>>,
    rx: OnceLock<oneshot::Receiver<PyResult<Py<PyAny>>>>,
}

impl Callback {
    pub(crate) fn new() -> Callback {
        let (tx, rx) = oneshot::channel::<PyResult<Py<PyAny>>>();
        Callback {
            tx: OnceLock::from(tx),
            rx: OnceLock::from(rx),
        }
    }
}

#[pymethods]
impl Callback {
    #[pyo3(signature = (*args))]
    fn __call__(&mut self, args: Py<PyTuple>) -> PyResult<()> {
        do_in!(|py| -> PyResult<_> {
            let (fut,) = args.extract::<(Py<PyAny>,)>(py)?;
            let result = fut.call_method0(py, "result")?;
            do_in!(|| self.tx.take()?.send(Ok(result)));
            info!("[Coroutine::send_result_with] Successfully sent result");
            Ok(())
        })
    }
}

#[pyclass]
pub(crate) struct Coroutine {
    coro: Py<PyAny>,
    callback: Option<Callback>,
}

#[pymethods]
impl Coroutine {
    #[pyo3(signature = ())]
    pub(crate) async fn __call__(&mut self) -> PyResult<Py<PyAny>> {
        match do_in!(|py| -> PyResult<_> {
            let event_loop = get_running_loop(py)?;
            let fut = ensure_future(py, &self.coro, Some(&event_loop))?;
            do_in!(|| info!("Ensured future: {}", fut.__str__()?));

            if let Some(mut callback) = self.callback.take() {
                let rx = callback.rx.take();
                match fut.call_method1("add_done_callback", (callback.into_py(py),)) {
                    Ok(res) => info!("[Coroutine::__call__] Added callback to future"),
                    Err(err) => {
                        error!("[Coroutine::__call__] Failed to add callback");
                        err.print_and_set_sys_last_vars(py)
                    }
                };
                PyResult::Ok(rx)
            } else {
                Err(PyRuntimeError::new_err("Nope"))
            }
        }) {
            Ok(Some(rx)) => {
                info!("Awaiting result");
                match rx.await {
                    Ok(Ok(result)) => {
                        do_in!(|| info!("Got result: {:#?}", result.__str__()?));
                        Ok(result)
                    }
                    Ok(Err(err)) => {
                        do_in!(|py| {
                            error!("Error awaiting result: {:#?}", err);
                            err.print(py)
                        });
                        Err(err)
                    }
                    Err(err) => Err(PyRuntimeError::new_err(format!(
                        "Error receiving result: {:#?}",
                        err
                    ))),
                }
            }
            Err(err) => {
                error!("Error: {:#?}", err);
                Err(err)
            }
            Ok(None) => {
                error!("We shouldn't be here");
                PyResult::Ok(do_in!(|py| py.None()))
            }
        }
    }
}

impl Coroutine {
    pub(crate) fn schedule(
        coro: Py<PyAny>,
        callback: Option<Callback>,
        cx: &AsyncAppContext,
    ) -> Task<PyResult<Py<PyAny>>> {
        do_in!(|| info!("Scheduling coroutine {:#?}", coro.__str__()?));

        cx.spawn(|_| async {
            match do_in!(|py| -> PyResult<_> {
                do_in!(|| info!("`coro`: {:#?}", coro.__str__()?));
                let coro = Coroutine { coro, callback }.into_py(py);

                let utils = py.import_bound("jupyter_core")?.getattr("utils")?;
                let event_loop = utils.getattr("ensure_event_loop")?.call0()?;
                do_in!(|| info!(
                    "[Coroutine::schedule] Got event loop: {}",
                    event_loop.__str__()?
                ));

                event_loop
                    .call_method1("run_until_complete", (coro.call0(py)?,))
                    .map(|obj| obj.unbind())
            }) {
                Ok(result) => Ok(result),
                Err(err) => {
                    do_in!(|py| err.print(py));
                    Err(err)
                }
            }
        })
    }
}
