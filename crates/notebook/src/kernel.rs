use anyhow::{anyhow, Result};
use collections::HashMap;
use futures::TryFutureExt;
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
//         Python::with_gil(|gil| {
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
        Python::with_gil(|py| -> Option<String> {
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
        Python::with_gil(|py| self.into_bound(py).map(|py_any| py_any.unbind()))
    }
}

pub struct JupyterKernelClient {
    command_tx: mpsc::Sender<KernelCommand>,
}

#[pyclass]
struct Awaitable {
    result_tx: Option<oneshot::Sender<Py<PyAny>>>,
    awaitable: Option<Py<PyAny>>,
    callback_rx: oneshot::Receiver<Py<PyAny>>,
}

#[pymethods]
impl Awaitable {
    #[pyo3(signature = ())]
    // pub fn __call__<'py>(&'py mut self) -> PyResult<&'py Bound<PyAny>> {
    pub fn __call__<'py>(&'py mut self) -> PyResult<Py<PyAny>> {
        do_in!(|py| {
            let ensure_future = py.import_bound("asyncio")?.getattr("ensure_future")?;
            let awaitable = self
                .awaitable
                .take()
                .ok_or_else(|| PyRuntimeError::new_err("nope"))?;

            info!("Got awaitable");

            let task = ensure_future.call1((awaitable,))?;
            info!("Ensured future");
            self.try_add_done_callback(task.unbind())
        })
    }

    fn try_add_done_callback<'py>(
        &'py mut self,
        // task: &Bound<'py, PyAny>,
        task: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        info!("Trying to get callback");
        let callback = self
            .callback_rx
            .try_recv()
            .map_err(|err| PyRuntimeError::new_err("Failed to receive callback"))?;
        info!("Got callback");
        do_in!(|py| task.call_method1(py, "add_done_callback", (callback,)))?;
        info!("Added callback");
        Ok(task)
    }

    fn send_result_with(&mut self, task: Py<PyAny>) -> PyResult<()> {
        let result = do_in!(|py| {
            debug_assert!(task.call_method0(py, "done")?.extract(py)?);
            task.into_py(py).call_method0(py, "result")
        })?;
        do_in!(|| self.result_tx.take()?.send(result));
        Ok(())
    }
}

impl Awaitable {
    fn schedule<'py>(
        awaitable: Py<PyAny>,
        event_loop: &Bound<'py, PyAny>,
    ) -> PyResult<oneshot::Receiver<Py<PyAny>>> {
        let (result_tx, result_rx) = oneshot::channel::<Py<PyAny>>();
        let (callback_tx, callback_rx) = oneshot::channel::<Py<PyAny>>();
        let awaitable = do_in!(|py| -> PyResult<_> {
            let awaitable = Awaitable {
                awaitable: Some(do_in!(|py| awaitable.into_py(py))),
                result_tx: Some(result_tx),
                callback_rx,
            }
            .into_py(py);
            info!("Created `Awaitable`");

            let callback = awaitable.getattr(py, "send_result_with")?;
            info!("Got `send_result_with` method");
            do_in!(|| -> Option<()> { callback_tx.send(callback).ok() });

            Ok(awaitable)
        })?;

        do_in!(|py| event_loop.into_py(py).call_method1(
            py,
            "run_until_complete",
            (awaitable.call0(py)?,),
        ))?;

        Ok(result_rx)
    }
}

impl JupyterKernelClient {
    pub async fn new(cx: &AsyncAppContext) -> Result<JupyterKernelClient> {
        let (tx, rx) = mpsc::channel::<KernelCommand>(1024);
        cx.spawn(|cx| async {
            JupyterKernelClient::task(rx).await;
        })
        .detach();

        let tx_clone = tx.clone();
        async move {
            let (tx, rx) = oneshot::channel::<Py<PyAny>>();
            let cmd = KernelCommand::Start { tx };
            tx_clone.send(cmd).await?;

            rx.await
                .map_err(|err| anyhow!("Failed to initialize required objects"))
        }
        .await?;
        info!("success");

        Ok(JupyterKernelClient { command_tx: tx })
    }

    async fn task(mut rx: mpsc::Receiver<KernelCommand>) -> Result<()> {
        loop {
            if let Some(cmd) = rx.recv().await {
                match cmd {
                    KernelCommand::Start { tx } => {
                        let rx = Python::with_gil(|py| -> PyResult<_> {
                            let asyncio = py.import_bound("asyncio")?;
                            let event_loop = asyncio.getattr("new_event_loop")?.call((), None)?;
                            // event_loop.call_method("run_forever", (), None)?;
                            asyncio.call_method1("set_event_loop", (&event_loop,))?;
                            do_in!(|| info!("Event loop: {:#?}", event_loop.__str__()?));

                            // event_loop.call_method0("run_forever")?;

                            // do_in!(|py| event_loop.call_method0(py, "run_forever"))?;
                            // do_in!(|| info!("Event loop: {:#?}", event_loop.__str__()?));

                            // do_in!(|py| event_loop.call_method_bound(py, "run_forever", (), None))?;
                            // info!("Started event loop");

                            let module_name = "jupyter_client";

                            // let ksm = PyPath::new(module_name, vec!["kernelspec", "KernelSpecManager"])
                            //     .into_py()?;
                            // do_in!(|| info!("Initialized `KernelSpecManager` {:#?}", ksm.__str__()?));

                            let km = Python::with_gil(|py| {
                                PyPath::new(module_name, vec!["manager", "AsyncKernelManager"])
                                    .into_py()
                                    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?
                                    .call_bound(py, (), None)
                            })?;
                            do_in!(|| info!(
                                "Initialized `AsyncKernelManager` {:#?}",
                                km.__str__()?
                            ));

                            let kernel_id = PyString::new_bound(py, "python3");
                            let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;
                            let coro =
                                km.call_method_bound(py, "start_kernel", (), Some(&kwargs))?;
                            info!("Got coro");

                            let rx = Awaitable::schedule(coro, &event_loop)?;

                            Ok(rx)
                        })
                        .map_err(forward_err_with(|err: PyErr| err.to_string()))?;

                        let res = rx.await?;
                        do_in!(|| info!("Result: {}", res.__str__()?));

                        // let kc = Python::with_gil(|py| -> PyResult<_> {
                        //     // TODO: Enable discovery and selection of kernels
                        //     let kernel_id = PyString::new_bound(py, "python3");
                        //     let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;

                        //     km.call_method_bound(py, "client", (), Some(&kwargs))
                        // })?;

                        // match tx.send(event_loop) {
                        //     Ok(_) => {}
                        //     Err(err) => {
                        //         error!("{err}");
                        //         break Ok(());
                        //     }
                        // };
                    }
                    _ => {}
                }
            };
        }
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
