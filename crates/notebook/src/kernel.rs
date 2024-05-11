use anyhow::{anyhow, Result};
use futures::{select, Future, FutureExt};
use gpui::{AsyncAppContext, EventEmitter, Flatten, Model, Task};
use log::{error, info};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyString, PyTuple};
use pyo3::{pyclass, Bound, Py, PyAny, PyResult, Python};
use ui::{Context, ViewContext};

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::cell::{Cell, CellId};
use crate::common::{forward_err_with, forward_with_print};
use crate::do_in;
use crate::editor::NotebookEditor;
use crate::jupyter;
use crate::jupyter::message::{IoPubSubMessageType, Message, MessageType};
use crate::jupyter::python::{Callback, Coroutine, Pydantic, TryAsStr};
use crate::kwargs;
use std::pin::Pin;

#[pyclass]
struct ForwardMessage {
    tx: mpsc::Sender<Message>,
}

impl ForwardMessage {
    fn new() -> (ForwardMessage, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(1024);
        (ForwardMessage { tx }, rx)
    }
}

pub struct JupyterKernelClient {
    // TODO: We'll want to ensure we can replace the sender in case the `JupyterKernelClient::task` dies
    command_tx: mpsc::Sender<KernelCommand>,
}

#[pymethods]
impl ForwardMessage {
    #[pyo3(signature = (py_msg))]
    fn __call__<'py>(&self, py_msg: &Bound<'py, PyAny>) -> PyResult<()> {
        do_in!(|| info!("Got message {:#?}", py_msg.__str__()?));
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

impl EventEmitter<KernelEvent> for JupyterKernelClient {}

impl JupyterKernelClient {
    pub async fn new_model(mut cx: AsyncAppContext) -> Result<Model<JupyterKernelClient>> {
        let (command_tx, command_rx) = mpsc::channel::<KernelCommand>(1024);
        let client = JupyterKernelClient { command_tx };
        let client_handle = cx.new_model(|cx| client)?;

        let cloned_client_handle = client_handle.clone();
        cx.spawn(|cx| async move {
            JupyterKernelClient::task(cloned_client_handle, command_rx, cx).await
        })
        .detach();

        client_handle
            .read_with(&cx, |client, _cx| client.ping_task())??
            .await?;

        Ok(client_handle)
    }

    pub(crate) fn run_cell(
        &self,
        cell: &Cell,
        cx: &ViewContext<NotebookEditor>,
    ) -> Result<(), mpsc::error::TrySendError<KernelCommand>> {
        let code = cell.source.read(cx).text_snapshot().text();
        self.send(KernelCommand::run(cell.id.get(), code))
    }

    fn send(&self, cmd: KernelCommand) -> Result<(), mpsc::error::TrySendError<KernelCommand>> {
        self.command_tx.try_send(cmd)
    }

    async fn task(
        client_handle: Model<JupyterKernelClient>,
        mut rx: mpsc::Receiver<KernelCommand>,
        mut cx: AsyncAppContext,
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

        // TODO: Since we can match on the deserialized `MessageType` enum, there's no need
        //       to set separate handlers for each channel.
        let (io_pubsub_handler, mut conn_rx) = ForwardMessage::new();
        if let Err(err) = do_in!(|py| {
            let io_pubsub_msg_type = py
                .import_bound("message")?
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
                maybe_msg = conn_rx.recv().fuse() => {
                    match maybe_msg {
                        Some(msg) => {
                            if let Err(err) = client_handle.update(&mut cx, |_client, cx| cx.emit(KernelEvent::ReceivedKernelMessage(msg))){
                                error!("Received message but failed to emit event: {:#?}", err);
                            }
                        },
                        None => continue
                    }
                }
                cmd = rx.recv().fuse() => {
                    use KernelCommand::*;
                    match cmd {
                        Some(Ping { tx }) => {
                            do_in!(|| tx.send(()).ok()?);
                        }
                        Some(Run { cell_id, code }) => {
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

    fn ping_task<'a, 'b: 'a>(&'a self) -> Result<Pin<Box<dyn Future<Output = Result<()>> + 'b>>>
where {
        let (ping_cmd, rx) = KernelCommand::ping();
        self.command_tx.try_send(ping_cmd)?;
        Ok(Box::pin(
            async move { rx.await.map_err(|err| anyhow!(err)) },
        ))
    }
}

#[derive(Debug)]
pub(crate) enum KernelCommand {
    Start { tx: oneshot::Sender<Py<PyAny>> },
    Stop {},
    Interrupt {},
    Run { cell_id: CellId, code: String },
    Ping { tx: oneshot::Sender<()> },
    Log { text: String },
}

impl KernelCommand {
    pub(crate) fn ping() -> (KernelCommand, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (KernelCommand::Ping { tx }, rx)
    }

    pub(crate) fn run(cell_id: CellId, code: String) -> KernelCommand {
        KernelCommand::Run { cell_id, code }
    }
}

pub enum KernelEvent {
    ReceivedKernelMessage(jupyter::message::Message),
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
