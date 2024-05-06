use anyhow::{anyhow, Result};
use collections::HashMap;
use futures::TryFutureExt;
use gpui::AsyncAppContext;
use itertools::Itertools;
use log::info;
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
use std::{
    future::Future,
    pin::{pin, Pin},
    task::{Context, Poll},
};
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
    kernel_client: Py<PyAny>,
    kernel_spec_manager: Py<PyAny>,
    async_kernel_manager: Py<PyAny>,
    // task: JoinHandle<Result<()>>,
    command_tx: mpsc::Sender<KernelCommand>,
    // kernel_registry: HashMap<KernelId, >
}

#[pyclass]
struct ResultSender {
    tx: Option<oneshot::Sender<Py<PyAny>>>,
    rx: Option<oneshot::Receiver<Py<PyAny>>>,
}

impl ResultSender {
    fn new() -> ResultSender {
        let (tx, rx) = oneshot::channel::<Py<PyAny>>();
        ResultSender {
            tx: Some(tx),
            rx: Some(rx),
        }
    }
}

#[pymethods]
impl ResultSender {
    fn try_send_result(&mut self, result: &Bound<'_, PyAny>) -> PyResult<()> {
        match do_in!(|| self.tx.take()?.send(result.clone().unbind()).ok()?) {
            Some(_) => Ok(()),
            None => Err(PyRuntimeError::new_err("Nope")),
        }
    }
}

impl JupyterKernelClient {
    pub async fn new(cx: &AsyncAppContext) -> Result<JupyterKernelClient> {
        let (tx, rx) = mpsc::channel::<KernelCommand>(1024);
        cx.spawn(|cx| async { JupyterKernelClient::task(rx).await })
            .detach();

        let module_name = "jupyter_client";

        let kernel_spec_manager_path: Vec<String> =
            vec!["kernelspec".into(), "KernelSpecManager".into()];
        let ksm = PyPath::new(module_name, kernel_spec_manager_path).into_py()?;
        do_in!(|| info!("Initialized `AsyncKernelManager` {:#?}", ksm.__str__()?));

        let mut handler = ResultSender::new();
        let rx = handler.rx.take().ok_or_else(|| anyhow!("Nope"))?;

        let km = PyPath::new(module_name, vec!["manager", "AsyncKernelManager"]).into_py()?;
        do_in!(|| info!("Initialized `AsyncKernelManager` {}", km.__str__()?));

        let kc = Python::with_gil(|py| -> PyResult<_> {
            // TODO: Enable discovery and selection of kernels
            let kernel_id = PyString::new_bound(py, "python3");
            let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;

            km.call_method_bound(py, "start_kernel", (&km,), Some(&kwargs))?;
            let kc = km.call_method_bound(py, "client", (&km,), Some(&kwargs))?;
            kc.call_method_bound(py, "wait_for_ready", (&kc,), None)?;

            Ok(kc)
        })?;
        do_in!(|| info!("Initialized `AsyncKernelManager` {}", kc.__str__()?));

        Ok(JupyterKernelClient {
            kernel_client: kc,
            kernel_spec_manager: ksm,
            async_kernel_manager: km,
            // task: task,
            command_tx: tx,
        })
    }

    async fn task(rx: mpsc::Receiver<KernelCommand>) -> Result<()> {
        let event_loop = Python::with_gil(|py| -> PyResult<_> {
            let asyncio = py.import_bound("asyncio")?;
            Ok(asyncio.getattr("run")?.call((), None)?.unbind())
        })
        .map_err(forward_err_with(|err: PyErr| err.to_string()))?;

        loop {}

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

enum KernelCommand {
    Start {},
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
