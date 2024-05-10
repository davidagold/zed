use anyhow::{anyhow, Result};
use futures::{select, FutureExt};
use gpui::{AsyncAppContext, Task};
use log::{error, info};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyString, PyTuple};
use pyo3::{pyclass, Bound, Py, PyAny, PyResult, Python};

use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::common::{forward_err_with, forward_with_print};
use crate::do_in;
use crate::jupyter::message::{IoPubSubMessageType, Message, MessageType};
use crate::jupyter::python::{Pydantic, TryAsStr};
use crate::kwargs;

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
    rx: OnceLock<oneshot::Receiver<PyResult<Py<PyAny>>>>,
}

impl Callback {
    fn new() -> Callback {
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
struct Coroutine {
    coro: Py<PyAny>,
    callback: Option<Callback>,
}

#[pymethods]
impl Coroutine {
    // #[pyo3(signature = (*args, **kwargs))]
    #[pyo3(signature = ())]
    async fn __call__(&mut self) -> PyResult<Py<PyAny>> {
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
    fn schedule(
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

#[pyclass]
struct MessageHandler {
    tx: mpsc::Sender<Message>,
}

impl MessageHandler {
    fn new() -> (MessageHandler, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(1024);
        (MessageHandler { tx }, rx)
    }
}

#[pymethods]
impl MessageHandler {
    #[pyo3(signature = (py_msg))]
    fn __call__<'py>(&self, py_msg: &Bound<'py, PyAny>) -> PyResult<()> {
        match Pydantic(py_msg).deserialize::<Message>() {
            Ok(msg) => self.tx.try_send(msg).map_err(|err| {
                PyRuntimeError::new_err(format!("Failed to send message: {:#?}", err.to_string()))
            }),
            Err(err) => Err(PyRuntimeError::new_err(format!(
                "Failed to deserialize message: {:#?}",
                err
            ))),
        }
    }
}

impl JupyterKernelClient {
    pub async fn new(cx: AsyncAppContext) -> Result<JupyterKernelClient> {
        let (tx, rx) = mpsc::channel::<KernelCommand>(1024);
        let cloned_tx = tx.clone();
        cx.spawn(|cx| async { JupyterKernelClient::task(rx, cloned_tx, cx).await })
            .detach();

        let (ping_cmd, rx) = KernelCommand::ping();
        tx.send(ping_cmd).await?;
        rx.await?;

        let run_cmd = KernelCommand::run("print('Hello World')".into());
        tx.send(run_cmd).await?;

        Ok(JupyterKernelClient { command_tx: tx })
    }

    async fn task(
        mut rx: mpsc::Receiver<KernelCommand>,
        command_tx: mpsc::Sender<KernelCommand>,
        cx: AsyncAppContext,
    ) -> anyhow::Result<()> {
        let conn = do_in!(|py| -> PyResult<_> {
            py.import_bound("connection")?
                .getattr("KernelConnection")?
                .call((), Some(&kwargs!(py, { "kernel_id" => "python3" })?))
                .map(|obj| obj.unbind())
        })
        .map_err(forward_err_with(|err: PyErr| err.to_string()))?;

        let task = match do_in!(|py| -> PyResult<Task<_>> {
            let coro = conn.call_method0(py, "start_kernel")?;
            Ok(Coroutine::schedule(coro, Some(Callback::new()), &cx))
        }) {
            Ok(task) => task,
            Err(err) => {
                error!("Failed to obtain receiver: {:#?}", err);
                do_in!(|py| err.print(py));
                return Err(anyhow!(
                    "Failed to call `KernelConnection.start_kernel`: {:#?}",
                    err
                ));
            }
        };

        let _ = task.await.map_err(forward_with_print::<()>);

        let listener = match do_in!(|py| -> PyResult<Task<_>> {
            let coro = conn.call_method0(py, "listen")?;
            Ok(Coroutine::schedule(coro, Some(Callback::new()), &cx))
        }) {
            Ok(task) => match task.await {
                Ok(listener) => listener,
                Err(err) => return forward_with_print(err),
            },
            Err(err) => return forward_with_print(err),
        };

        let (io_pubsub_handler, mut conn_rx) = MessageHandler::new();
        if let Err(err) = do_in!(|py| {
            let io_pubsub_msg_type = py
                .import_bound("connection")?
                .getattr("IoPubSubChannelMessage")?;
            let args = (io_pubsub_msg_type, io_pubsub_handler);
            conn.call_method1(py, "set_message_handler", args)
        }) {
            error!("Failed to add message handler for IO PubSub channel");
            do_in!(|py| err.print(py));
        }

        info!("[JupyterKernelClient task] Starting inner loop");
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(5))
                .await;
            do_in!(|py| -> Option<_> {
                Some(listener.call_method0(py, "running").ok()?.__str__()?)
            });

            select! {
                msg = conn_rx.recv().fuse() => {
                    let Some(msg) = msg else {
                        continue
                    };
                    use MessageType::*;
                    use IoPubSubMessageType::*;
                    match msg.msg_type {
                        IoPubSub(Stream) => {
                            info!("{:#?}", msg);
                        },
                        _ => continue
                    }
                }
                cmd = rx.recv().fuse() => {
                    use KernelCommand::*;
                    match cmd {
                        Some(Ping { tx }) => {
                            do_in!(|| tx.send(()).ok()?);
                        }
                        Some(Run { code }) => {
                            do_in!(|py| {
                                match conn.call_method1(py, "execute_code", (code,)) {
                                    Ok(response) => {
                                        do_in!(|| info!("Message ID: {:#?}", response.__str__()?));
                                    }
                                    Err(err) => err.print(py),
                                }
                            });
                        }
                        Some(Log { text }) => {
                            info!("Log message: {}", text);
                        }
                        _ => {}
                    }
                }
            };
        }
    }
}

// TODO: Decide whether to include these or save them for later
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
    Log { text: String },
}

impl KernelCommand {
    fn ping() -> (KernelCommand, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (KernelCommand::Ping { tx }, rx)
    }

    fn run(code: String) -> KernelCommand {
        KernelCommand::Run { code }
    }
}
