use anyhow::{anyhow, Result};
use collections::HashMap;
use futures::channel::mpsc::SendError;
use futures::{Future, TryFutureExt};
use gpui::AsyncAppContext;
use itertools::Itertools;
use log::{error, info};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyNone;
use pyo3::{
    pyclass,
    types::{PyAnyMethods, PyDict, PyString},
    Bound, Py, PyAny, PyResult, Python,
};
use runtimelib::media::MimeType;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::common::forward_err_with;
use crate::{
    cell::{MimeData, StreamOutputTarget},
    do_in,
};

// // See https://pyo3.rs/v0.21.0/async-await#release-the-gil-across-await
// struct AllowThreads<F>(F);

// impl<F> Future for AllowThreads<F>
// where
//     F: Future + Unpin + Send,
//     F::Output: Send,
// {
//     type Output = F::Output;

//     fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//         let waker = cx.waker();
//         pyo3::with_embedded_python_interpreter(|gil| {
//             gil.allow_threads(|| pin!(&mut self.0).poll(&mut Context::from_waker(waker)))
//         })
//     }
// }

macro_rules! kwargs {
    ($py:ident, { $( $key:literal => $val:ident $(,)? )* }) => {
        {
            (|py| -> PyResult<Bound<'_, PyDict>> {
                fn __kwargs<'py>(py: Python<'py>) -> Bound<'py, PyDict> {
                    PyDict::new_bound(py)
                }
                let kwargs = __kwargs(py);
                $(
                    let key = PyString::new_bound(py, $key);
                    kwargs.set_item(key, $val)?;
                )*

                Ok(kwargs)
            })($py)
        }
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

#[derive(Debug, Clone, Default)]
pub struct PyPath {
    module: String,
    path: Vec<String>,
}

impl PyPath {
    pub fn new<T: Into<String>>(module: &str, path: Vec<T>) -> PyPath {
        PyPath {
            module: module.to_string(),
            path: path.into_iter().map(|attr| attr.into()).collect_vec(),
        }
    }

    pub fn into_bound<'py>(self, py: Python<'py>) -> anyhow::Result<Bound<'py, PyAny>> {
        let module = py.import_bound(PyString::new_bound(py, self.module.as_str()))?;
        self.path
            .into_iter()
            .try_fold(module.as_any().clone(), |obj, attr| {
                //
                obj.getattr(PyString::new_bound(py, attr.as_str()))
            })
            .map_err(|err| anyhow!(err))
    }

    pub fn into_py(self) -> anyhow::Result<Py<PyAny>> {
        do_in!(|py| { self.into_bound(py).map(|py_any| py_any.unbind()) })
    }
}

pub struct JupyterKernelClient {
    command_tx: mpsc::Sender<KernelCommand>,
}

#[pyclass]
struct Coroutine {
    awaitable: Py<PyAny>,
    tx: Option<oneshot::Sender<PyResult<Py<PyAny>>>>,
}

#[pymethods]
impl Coroutine {
    #[pyo3(signature = ())]
    fn __call__(&self, py: Python) -> PyResult<Py<PyAny>> {
        py.import_bound("asyncio")?
            .getattr("ensure_future")?
            .call1((self.awaitable.bind(py),))
            .map(|aw| aw.unbind())
    }

    fn try_add_done_callback<'py>(
        &'py self,
        py: Python<'py>,
        callback: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        self.awaitable
            .call_method1(py, "add_done_callback", (callback,))
    }

    fn send_result_with<'py>(&mut self, awaitable: &Bound<'_, PyAny>) -> PyResult<()> {
        let result = awaitable.call_method0("result")?.unbind();
        do_in!(|| self.tx.take()?.send(Ok(result)));
        Ok(())
    }
}

impl Coroutine {
    fn schedule<'py>(
        py: Python<'py>,
        coro: Py<PyAny>,
        event_loop: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<oneshot::Receiver<PyResult<Py<PyAny>>>> {
        let asyncio = py.import_bound("asyncio")?;
        let awaitable = asyncio.getattr("ensure_future")?.call1((coro,))?.unbind();
        let add_done_callback = py
            .import_bound("functools")?
            .getattr("partial")?
            .call1((awaitable.getattr(py, "add_done_callback")?,))?;

        let (tx, rx) = oneshot::channel::<PyResult<Py<PyAny>>>();
        let coro = Coroutine {
            awaitable,
            tx: Some(tx),
        }
        .into_py(py);

        let callback = coro.getattr(py, "send_result_with")?;

        add_done_callback.call1((callback,))?;
        let event_loop = match event_loop {
            Some(event_loop) => event_loop.clone().unbind(),
            None => asyncio.getattr("get_running_loop")?.call0()?.unbind(),
        };
        event_loop.call_method1(py, "call_soon_threadsafe", (coro.call0(py)?,))?;

        Ok(rx)
    }
}

#[derive(Debug)]
struct KernelComponents {
    kernelspec_manager: Py<PyAny>,
    kernel_manager: Py<PyAny>,
    // kernel_client: Py<PyAny>,
}

impl JupyterKernelClient {
    pub async fn new(cx: &AsyncAppContext) -> Result<JupyterKernelClient> {
        let (tx, rx) = mpsc::channel::<KernelCommand>(1024);
        cx.spawn(|cx| async {
            JupyterKernelClient::task(rx).await;
        })
        .detach();

        // let tx_clone = tx.clone();
        // async move {
        //     let (tx, rx) = oneshot::channel::<Py<PyAny>>();
        //     let cmd = KernelCommand::Start { tx };
        //     tx_clone.send(cmd).await?;

        //     rx.await
        //         .map_err(forward_err_with(|err: RecvError| err.to_string()))?;
        //     anyhow::Ok(())
        // }
        // .await?;
        // info!("success");

        let (tx_init, rx_init) = oneshot::channel::<KernelComponents>();
        if let Err(err) = JupyterKernelClient::init(tx_init, cx).await {
            error!("Failed to initialize Jupyter kernel client: {:#?}", err);
        };

        let kernel = match rx_init.await {
            Ok(kernel) => kernel,
            Err(err) => {
                let err = anyhow!("Failed to initialize Jupyter kernel client: {:#?}", err);
                error!("{:#?}", err);
                return Err(err);
            }
        };

        info!("{:#?}", kernel);
        Ok(JupyterKernelClient { command_tx: tx })
    }

    async fn init(
        tx: oneshot::Sender<KernelComponents>,
        cx: &AsyncAppContext,
    ) -> anyhow::Result<()> {
        let rx = do_in!(|py| -> anyhow::Result<_> {
            do_in!(|| -> PyResult<()> {
                let asyncio = py.import_bound("asyncio")?.unbind();
                let event_loop = asyncio
                    .getattr(py, "new_event_loop")?
                    .call_bound(py, (), None)?;
                do_in!(|| info!("Event loop: {:#?}", event_loop.__str__()?));

                unsafe {
                    cx.spawn(|_| async move {
                        Python::with_gil_unchecked(|py| {
                            info!("Can we see this?");
                            asyncio.call_method1(py, "set_event_loop", (&event_loop,))?;
                            event_loop.call_method0(py, "run_forever")?;
                            PyResult::Ok(())
                        })
                        .map_err(forward_err_with(|err: PyErr| err.to_string()))?;
                        anyhow::Ok(())
                    })
                    .detach();
                };
                Ok(())
            })
            .map_err(forward_err_with(|err: PyErr| err.to_string()))?;
            // rx
            let (ksm, km) = do_in!(|| -> PyResult<_> {
                let jupyter_client = py.import_bound("jupyter_client")?;

                let ksm = jupyter_client
                    .getattr("kernelspec")?
                    .getattr("KernelSpecManager")?
                    .call0()?;
                do_in!(|| info!(
                    "Initialized `KernelSpecManager` {:#?}",
                    ksm.call_method0("get_all_specs").ok()?
                ));

                let km = jupyter_client
                    .getattr("manager")?
                    .getattr("AsyncKernelManager")?
                    .call0()?;
                do_in!(|| info!("Initialized `AsyncKernelManager` {:#?}", km.__str__()?));

                Ok((ksm, km))
            })
            .map_err(forward_err_with(|err: PyErr| err.to_string()))?;

            let kernel_id = PyString::new_bound(py, "python3");
            let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;
            let coro = km.call_method("start_kernel", (), Some(&kwargs))?;
            info!("Got coro");

            match tx.send(KernelComponents {
                kernelspec_manager: ksm.unbind(),
                kernel_manager: km.unbind(),
            }) {
                Ok(_) => {}
                Err(err) => {
                    error!("Failed to send components: {:#?}", err)
                }
            };

            Coroutine::schedule(py, coro.unbind(), None)
                .map_err(forward_err_with(|err: PyErr| err.to_string()))
        })?;

        let res = rx
            .await
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))??;

        do_in!(|| info!("Result: {}", res.__str__()?));

        Ok(())
        // .map_err(forward_err_with(|err: PyErr| err.to_string()))?;
    }

    async fn task(mut command_rx: mpsc::Receiver<KernelCommand>) -> Result<()> {
        // let startup_rx =

        // do_in!(|py| {
        //     let components = KernelComponents {
        //         event_loop: event_loop.unbind(),
        //         kernelspec_manager: ksm.unbind(),
        //         kernel_manager: km.unbind(),
        //         kernel_client: kc,
        //     };

        //     Ok(())
        // })?;

        // loop {
        //     if let Some(cmd) = command_rx.recv().await {
        //         match cmd {
        //             KernelCommand::Start { tx } => {}
        //             _ => {}
        //         }
        //     };
        // }
        Ok(())
    }
}

struct KernelId(String);

enum KernelState {
    Stopped,
    Starting,
    Ready,
    Executing,
    Responding,
    Terminating,
}

#[derive(Debug)]
enum KernelCommand {
    Start { tx: oneshot::Sender<Py<PyAny>> },
    Stop {},
    Interrupt {},
    Execute {},
}

struct JupyterMessageHeader {
    msg_id: String,
    session: String,
    username: String,
    date: String,
    msg_type: String,
    version: String,
}

struct JupyterMessage {
    header: JupyterMessageHeader,
    msg_id: String,
    msg_type: String,
    parent_header: JupyterMessageHeader,
    metadata: HashMap<String, Value>,
    content: HashMap<String, Value>,
    buffers: Vec<Vec<u8>>,
}

#[derive(Deserialize)]
enum JupyterMessageContent {
    #[serde(alias = "stream")]
    Stream {
        name: StreamOutputTarget,
        text: String,
    },
    #[serde(alias = "display_data")]
    DisplayData {
        data: HashMap<MimeType, MimeData>,
        metadata: HashMap<MimeType, Value>,
        transient: HashMap<String, Value>,
    },
    #[serde(alias = "execute_input")]
    ExecutionInput {
        code: String,
        execution_count: usize,
    },
    #[serde(alias = "execute_result")]
    ExecutionResult {
        execution_count: usize,
        data: HashMap<MimeType, MimeData>,
        metadata: HashMap<MimeType, Value>,
    },
}
