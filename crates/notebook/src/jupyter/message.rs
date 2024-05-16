use collections::HashMap;
use runtimelib::media::MimeType;
use serde::Deserializer;
use serde::{
    de::{Error, Visitor},
    Deserialize,
};
use serde_json::Value;

use crate::cell::{MimeData, StreamOutputTarget};

#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    pub(crate) header: MessageHeader,
    pub(crate) msg_id: String,
    pub(crate) msg_type: MessageType,
    pub(crate) parent_header: MessageHeader,
    pub(crate) metadata: HashMap<String, Value>,
    pub(crate) content: HashMap<String, Value>,
    pub(crate) buffers: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MessageHeader {
    pub(crate) msg_id: String,
    pub(crate) session: String,
    pub(crate) username: String,
    pub(crate) date: Option<String>,
    pub(crate) msg_type: MessageType,
    pub(crate) version: String,
}

#[derive(Clone, Debug)]
pub enum MessageType {
    Shell(ShellMessageType),
    IoPubSub(IoPubSubMessageType),
}

impl From<ShellMessageType> for MessageType {
    fn from(msg_type: ShellMessageType) -> Self {
        MessageType::Shell(msg_type)
    }
}

impl From<IoPubSubMessageType> for MessageType {
    fn from(msg_type: IoPubSubMessageType) -> Self {
        MessageType::IoPubSub(msg_type)
    }
}

struct MessageTypeVisitor();

impl<'de> Visitor<'de> for MessageTypeVisitor {
    type Value = MessageType;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "A Jupyter message type")
    }

    fn visit_str<E>(self, v: &str) -> std::prelude::v1::Result<Self::Value, E>
    where
        E: Error,
    {
        if let Ok(msg_type) =
            serde_json::from_value::<ShellMessageType>(Value::String(v.to_string()))
        {
            return Ok(msg_type.into());
        }
        if let Ok(msg_type) =
            serde_json::from_value::<IoPubSubMessageType>(Value::String(v.to_string()))
        {
            return Ok(msg_type.into());
        }
        Err(E::custom("Failed to deserialize message type"))
    }
}

impl<'de> Deserialize<'de> for MessageType {
    fn deserialize<D>(deserializer: D) -> std::prelude::v1::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(MessageTypeVisitor())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub enum ShellMessageType {
    #[serde(rename = "execute_request")]
    ExecuteRequest,
    #[serde(rename = "execute_reply")]
    ExecuteReply,
    #[serde(rename = "inspect_request")]
    InspectRequest,
    #[serde(rename = "inspect_reply")]
    InspecyReply,
    #[serde(rename = "complete_request")]
    CompletedProquest,
    #[serde(rename = "complete_reply")]
    CompleteReply,
    #[serde(rename = "history_request")]
    HistoryRequest,
    #[serde(rename = "history_reply")]
    HistoryReply,
    #[serde(rename = "is_complete_request")]
    IsCompleteRequest,
    #[serde(rename = "is_complete_reply")]
    IsCompleteReply,
    #[serde(rename = "connect_request")]
    ConnectRequest,
    #[serde(rename = "connect_reply")]
    ConnectReply,
    #[serde(rename = "comm_info_request")]
    CommInfoRequest,
    #[serde(rename = "comm_info_reply")]
    CommInfoReply,
    #[serde(rename = "kernel_info_request")]
    KernelInfoRequest,
    #[serde(rename = "kernel_info_reply")]
    KernelInfoReply,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub enum IoPubSubMessageType {
    #[serde(rename = "stream")]
    Stream,
    #[serde(rename = "display_data")]
    DisplayData,
    #[serde(rename = "update_display_data")]
    UpdateDisplayData,
    #[serde(rename = "execute_input")]
    ExecuteInput,
    #[serde(rename = "execute_result")]
    ExecuteResult,
    #[serde(rename = "error")]
    ExecutionError,
    #[serde(rename = "status")]
    KernelStatus,
    #[serde(rename = "clear_output")]
    ClearOutput,
    #[serde(rename = "debug_event")]
    DebugEvent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum IoPubSubMessageContent {
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
