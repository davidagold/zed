use std::borrow::{Borrow, BorrowMut};
use std::cell::Cell;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use collections::HashMap;
use futures::Future;
use gpui::AsyncAppContext;
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

// // See https://pyo3.rs/v0.21.0/async-await#release-the-gil-across-await
struct AllowThreads<F>(F);

impl<F> Future for AllowThreads<F>
where
    F: Future + Unpin + Send,
    F::Output: Send,
{
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let waker = cx.waker();
        Python::with_gil(|gil| {
            gil.allow_threads(|| pin!(&mut self.0).poll(&mut Context::from_waker(waker)))
        })
    }
}

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

#[derive(Debug, Clone, Default)]
pub struct PyPath {
    module: String,
    path: Vec<String>,
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

fn partial<'py>(
    py: Python<'py>,
    args: impl IntoPy<Py<PyTuple>>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Bound<'py, PyAny>> {
    py.import_bound("functools")?
        .getattr("partial")?
        .call(args, kwargs)
}

#[pyclass]
struct Coroutine {
    coro: Py<PyAny>,
    tx: OnceLock<mpsc::Sender<PyResult<Py<PyAny>>>>,
    callback: OnceLock<Py<PyAny>>,
    chain_fn: OnceLock<Py<PyAny>>,
}

fn if_ok<T: IntoPy<Py<PyAny>>>(result: PyResult<T>) -> PyResult<T> {
    match result {
        Ok(obj) => Ok(obj),
        Err(err) => {
            do_in!(|py| err.print(py));
            Err(err)
        }
    }
}

#[pymethods]
impl Coroutine {
    #[pyo3(signature = (*args, **kwargs))]
    async fn __call__(&self, args: Py<PyTuple>, kwargs: Option<Py<PyDict>>) -> PyResult<()> {
        let mut rx = do_in!(|py| {
            info!("[Coroutine::__call__] Called");
            let event_loop = if_ok(get_running_loop(py))?;
            let fut = if_ok(ensure_future(py, &self.coro, Some(&event_loop)))?;

            do_in!(|| info!(
                "[Coroutine::__call__]: Ensured coroutine is future: {}",
                fut.__str__()?
            ));
            // do_in!(|| info!(
            //     "[Coroutine::__call__]: Chain fn: {}",
            //     self.chain_fn.as_ref()?.__str__()?
            // ));
            //
            let (tx, rx) = mpsc::channel::<PyResult<Py<PyAny>>>(1);
            self.tx.set(tx);

            match do_in!(|| fut.call_method1("add_done_callback", (self.callback.get()?,))) {
                Some(Ok(res)) => info!("[Coroutine::__call__] Added callback to future"),
                Some(Err(err)) => {
                    error!("[Coroutine::__call__] Failed to add callback");
                    err.print_and_set_sys_last_vars(py)
                }
                None => error!("Callback was not set"),
            };

            // do_in!(|| {
            //     info!("[Coroutine::__call__] future: {:#?}", fut.__str__()?);
            //     if let Err(err) = self.chain_fn.get()?.call1(py, (fut,)) {
            //         error!("Error chaining future:");
            //         err.print(py)
            //     };
            // });
            PyResult::Ok(rx)
            // Ok(self.coro.bind(py).clone().unbind())
        })?;

        let result = rx
            .recv()
            .await
            .ok_or_else(|| PyRuntimeError::new_err("Nope"))??;
        // .map_err(|err| PyRuntimeError::new_err("Nope"))??;
        do_in!(|| info!("{:#?}", result.__str__()?));
        Ok(())
    }

    fn set_done_callback(&self, callback: Py<PyAny>) -> PyResult<()> {
        if let Err(err) = self.callback.set(callback) {
            error!("Failed to set done callback");
        };
        Ok(())
    }

    fn set_chain_fn(&self, chain_fn: Py<PyAny>) -> PyResult<()> {
        self.chain_fn.set(chain_fn);
        Ok(())
    }

    fn send_result_with(&self, awaitable: &Bound<'_, PyAny>) -> PyResult<()> {
        let result = awaitable.call_method0("result")?.unbind();
        do_in!(|| self.tx.get()?.send(Ok(result)));
        info!("[Coroutine::send_result_with] Successfully sent result");
        Ok(())
    }
}

impl Coroutine {
    fn schedule(
        coro: Py<PyAny>,
        // cx: &AsyncAppContext,
        // ) -> PyResult<oneshot::Receiver<PyResult<Py<PyAny>>>> {
    ) -> PyResult<()> {
        do_in!(|| info!("Scheduling coroutine {:#?}", coro.__str__()?));

        do_in!(|py| -> PyResult<_> {
            let asyncio = py.import_bound("asyncio")?;

            // let (tx, rx) = oneshot::channel::<PyResult<Py<PyAny>>>();
            do_in!(|| info!("`coro`: {:#?}", coro.__str__()?));
            let coro = Coroutine {
                coro,
                tx: OnceLock::new(),
                callback: OnceLock::new(),
                chain_fn: OnceLock::new(),
            }
            .into_py(py);

            let callback = coro.getattr(py, "send_result_with")?;
            coro.call_method1(py, "set_done_callback", (callback,))?;

            let utils = py.import_bound("jupyter_core")?.getattr("utils")?;
            let event_loop = utils.getattr("ensure_event_loop")?.call0()?;
            do_in!(|| info!(
                "[Coroutine::schedule] Got event loop: {}",
                event_loop.__str__()?
            ));

            // let fut = asyncio.call_method(
            //     "run_coroutine_threadsafe",
            //     (coro.call0(py)?,),
            //     // None,
            //     Some(&kwargs!(py, { "loop" => event_loop })?),
            // )?;
            // do_in!(|| info!("{:#?}", fut.get_type()));

            // do_in!(|| info!("Coroutine.__call__ returned: {:#?}", fut.__str__()?));
            event_loop.call_method1(
                "run_until_complete",
                (coro.call0(py)?,),
                // (asyncio.getattr("wrap_future")?.call1((fut,))?,),
            )?;
            // py.import_bound("asyncio")?
            // .getattr("run_until_complete")?
            // .call1((fut,)?;
            info!("Submitted future");
            // Ok(rx)
            Ok(())
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
            // let jupyter_client = py.import_bound("jupyter_client")?;

            // let ksm = jupyter_client
            //     .getattr("kernelspec")?
            //     .getattr("KernelSpecManager")?
            //     .call0()?;
            // do_in!(|| info!(
            //     "Initialized `KernelSpecManager` {:#?}",
            //     ksm.call_method0("get_all_specs").ok()?
            // ));

            // let km = jupyter_client
            //     .getattr("manager")?
            //     .getattr("KernelManager")?
            //     .call0()?;
            // do_in!(|| info!("Initialized `KernelManager` {:#?}", km.__str__()?));

            // let async_km = jupyter_client
            //     .getattr("manager")?
            //     .getattr("AsyncKernelManager")?
            //     .call0()?;
            // do_in!(|| info!("Initialized `KernelManager` {:#?}", km.__str__()?));

            // Ok((ksm.unbind(), km.unbind(), async_km.unbind()))
        })
        .map_err(forward_err_with(|err: PyErr| err.to_string()))?;

        let task_rx = match do_in!(|py| -> PyResult<_> {
            let coro = conn.call_method0(py, "start_kernel")?;
            Coroutine::schedule(coro)
        }) {
            Ok(task_rx) => {
                info!("Got task receiver");
                task_rx
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

        // match task_rx.await {
        //     Ok(Ok(_)) => {
        //         info!("Something");
        //     }
        //     Ok(Err(err)) => do_in!(|py| err.print(py)),
        //     Err(err) => {
        //         error!("Error awaiting `KernelConnection.start_kernel`: {:#?}", err);
        //     }
        // };
        // info!("Kernel ready.");

        // let async_kc = match do_in!(|py| -> PyResult<_> {
        //     let kernel_id = PyString::new_bound(py, "python3");
        //     let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;
        //     async_km.call_method_bound(py, "client", (), Some(&kwargs))
        // }) {
        //     Ok(kc) => kc,
        //     Err(err) => {
        //         let err = anyhow!("Failed to obtain async kernel client: {:#?}", err);
        //         error!("{:#?}", err);
        //         return Err(err);
        //     }
        // };

        // match do_in!(|py| {
        //     let kernel_id = PyString::new_bound(py, "python3");
        //     let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;
        //     km.call_method_bound(py, "start_kernel", (), Some(&kwargs))
        // }) {
        //     Ok(response) => do_in!(|| info!("Kernel started? {:#?}", response.__str__()?)),
        //     Err(err) => do_in!(|py| Some(err.print(py))),
        // };

        // let kc = do_in!(|py| -> PyResult<_> {
        //     let kernel_id = PyString::new_bound(py, "python3");
        //     let kwargs = kwargs!(py, { "kernel_id" => kernel_id })?;
        //     let kc = km.call_method_bound(py, "client", (), Some(&kwargs))?;
        //     kc.call_method0(py, "start_channels")?;
        //     kc.call_method0(py, "wait_for_ready")?;
        //     info!("Started synchronous kernel client");
        //     Ok(kc)
        // })?;
        //
        // if let Err(err) = do_in!(|py| async_kc.call_method0(py, "start_channels")) {
        //     error!("Failed to start kernel client channels:");
        //     do_in!(|py| err.print(py));
        // };
        // let coro = do_in!(|py| async_kc.call_method0(py, "wait_for_ready"))?;
        // if let Err(err) = Coroutine::schedule(coro).await {
        //     do_in!(|py| err.print(py));
        // };

        // let kwargs = kwargs!(py, { "timeout" => 5 })?;
        // cloned_async_kc.call_method_bound(py, "get_iopub_msg", (), Some(&kwargs))
        //
        // let module = py.import_bound("test_server")?.getattr("KernelConnection")?;

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
