use std::borrow::{Borrow, BorrowMut};
use std::cell::Cell;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use collections::HashMap;
use futures::Future;
use gpui::{AsyncAppContext, Task};
use log::{error, info};
use pyo3::exceptions::{PyAttributeError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3::{
    pyclass,
    types::{PyAnyMethods, PyDict, PyString},
    Bound, Py, PyAny, PyResult, Python,
};
use runtimelib::media::MimeType;
use serde::Deserialize;
use serde_json::Value;
use std::pin::{pin, Pin};
use tokio::sync::{mpsc, oneshot};

use crate::common::forward_err_with;
use crate::{
    cell::{MimeData, StreamOutputTarget},
    do_in,
};

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

pub struct JupyterKernelClient {
    command_tx: mpsc::Sender<KernelCommand>,
}

fn get_running_loop<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    py.import_bound("asyncio")?
        .getattr("get_running_loop")?
        .call0()
}

fn ensure_future<'py>(
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
struct Callback {
    tx: OnceLock<oneshot::Sender<PyResult<Py<PyAny>>>>,
}

#[pymethods]
impl Callback {
    #[pyo3(signature = (*args, **kwargs))]
    fn __call__(&mut self, args: Py<PyTuple>, kwargs: Option<Py<PyDict>>) -> PyResult<()> {
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
struct Coroutine {
    coro: Py<PyAny>,
}

#[pymethods]
impl Coroutine {
    #[pyo3(signature = (*args, **kwargs))]
    async fn __call__(&self, args: Py<PyTuple>, kwargs: Option<Py<PyDict>>) -> PyResult<()> {
        let rx = do_in!(|py| {
            info!("[Coroutine::__call__] Called");
            let event_loop = get_running_loop(py)?;
            let fut = ensure_future(py, &self.coro, Some(&event_loop))?;

            let (tx, rx) = oneshot::channel::<PyResult<Py<PyAny>>>();
            let callback = Callback {
                tx: OnceLock::from(tx),
            }
            .into_py(py);

            match do_in!(|| fut.call_method1("add_done_callback", (callback,))) {
                Some(Ok(res)) => info!("[Coroutine::__call__] Added callback to future"),
                Some(Err(err)) => {
                    error!("[Coroutine::__call__] Failed to add callback");
                    err.print_and_set_sys_last_vars(py)
                }
                None => error!("Callback was not set"),
            };

            PyResult::Ok(rx)
        })?;

        let result = rx.await.map_err(|err| {
            error!("Nope: {:#?}", err);
            PyRuntimeError::new_err(err.to_string())
        })??;
        do_in!(|| info!("{:#?}", result.__str__()?));
        Ok(())
    }
}

impl Coroutine {
    fn schedule(coro: Py<PyAny>, cx: &AsyncAppContext) -> Task<PyResult<Py<PyAny>>> {
        do_in!(|| info!("Scheduling coroutine {:#?}", coro.__str__()?));

        cx.spawn(|cx| async {
            do_in!(|py| -> PyResult<_> {
                // let asyncio = py.import_bound("asyncio")?;
                // let (tx, rx) = oneshot::channel::<PyResult<Py<PyAny>>>();

                do_in!(|| info!("`coro`: {:#?}", coro.__str__()?));
                let coro = Coroutine { coro }.into_py(py);

                let utils = py.import_bound("jupyter_core")?.getattr("utils")?;
                let event_loop = utils.getattr("ensure_event_loop")?.call0()?;
                do_in!(|| info!(
                    "[Coroutine::schedule] Got event loop: {}",
                    event_loop.__str__()?
                ));

                event_loop
                    .call_method1("run_until_complete", (coro.call0(py)?,))
                    .map(|obj| obj.unbind())
            })
        })
    }

    // fn spawn(coro: Py<PyAny>, cx: &AsyncAppContext) -> PyResult<()> {
    //     cx.spawn(|cx| async {
    //         let Ok(task_rx) = Coroutine::schedule(coro) else {
    //             return;
    //         };
    //         let _ = task_rx.await;
    //     })
    //     .detach();
    //     Ok(())
    // }

    fn run_sync(coro: Py<PyAny>, py: Python) -> PyResult<Py<PyAny>> {
        let utils = py.import_bound("jupyter_core")?.getattr("utils")?;
        utils
            .getattr("run_sync")?
            .call1((coro,))?
            .call0()
            .map(|obj| obj.unbind())
    }
}

impl JupyterKernelClient {
    pub async fn new(cx: AsyncAppContext) -> Result<JupyterKernelClient> {
        let (tx, rx) = mpsc::channel::<KernelCommand>(1024);
        let cloned_tx = tx.clone();
        cx.spawn(|cx| async { JupyterKernelClient::task(rx, cloned_tx, cx).await })
            .detach();

        Ok(JupyterKernelClient { command_tx: tx })
    }

    async fn task(
        mut rx: mpsc::Receiver<KernelCommand>,
        command_tx: mpsc::Sender<KernelCommand>,
        cx: AsyncAppContext,
    ) -> anyhow::Result<()> {
        let conn = do_in!(|py| -> PyResult<_> {
            py.import_bound("test_server")?
                .getattr("KernelConnection")?
                .call((), Some(&kwargs!(py, { "kernel_id" => "python3" })?))
                .map(|obj| obj.unbind())
        })
        .map_err(forward_err_with(|err: PyErr| err.to_string()))?;

        let task = match do_in!(|py| -> PyResult<Task<_>> {
            let coro = conn.call_method0(py, "start_kernel")?;
            Ok(Coroutine::schedule(coro, &cx))
        }) {
            Ok(task) => {
                info!("Got task receiver");
                task
            }
            Err(err) => {
                error!("Failed to obtain receiver: {:#?}", err);
                do_in!(|py| err.print(py));
                return Err(anyhow!(
                    "Failed to call `KernelConnection.start_kernel`: {:#?}",
                    err
                ));
            }
        };

        let _ = task.await;

        info!("[JupyterKernelClient task] Starting inner loop");
        loop {
            info!("Waiting for commands...");
            if let Some(cmd) = rx.recv().await {
                use KernelCommand::*;
                match cmd {
                    Ping { tx } => {
                        do_in!(|| tx.send(()).ok()?);
                        info!("Pong");
                    }
                    Run { code } => {
                        info!("Received code: {}", code);
                        // let msg_id = do_in!(|py| async_kc.call_method1(py, "execute", (code,)))?;
                        // do_in!(|| info!(
                        //     "Submitted execution request with message id: {}",
                        //     msg_id.__str__()?
                        // ));
                    }
                    Log { text } => {
                        info!("Log message: {}", text);
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
    Run { code: String },
    Ping { tx: oneshot::Sender<()> },
    // Log { rx: oneshot::Sender<String> },
    Log { text: String },
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
