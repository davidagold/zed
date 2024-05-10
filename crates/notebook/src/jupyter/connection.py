import asyncio
import logging
import signal
import sys

from abc import ABC, ABCMeta
from asyncio.taskgroups import TaskGroup
from asyncio.tasks import sleep
from concurrent.futures import ThreadPoolExecutor, Future
from datetime import datetime
from enum import Enum
from functools import lru_cache, partial, reduce, wraps
from typing import Any, TypeVar, Dict, Iterable, Iterator, List, Literal, Optional, Protocol, Tuple, Type, get_type_hints, Union, Callable

from jupyter_client.asynchronous.client import AsyncKernelClient
from jupyter_client.kernelspec import KernelSpecManager
from jupyter_client.manager import AsyncKernelManager, KernelManager
from pydantic import (
    BaseModel,
    ConfigDict,
    field_serializer,
    model_validator,
)
from pydantic.functional_validators import BeforeValidator, field_validator

try:
    import rich
except ModuleNotFoundError:
    logging.warning("Module `rich` not available, printing will not be pretty")

from typing_extensions import Annotated, ParamSpec, TypeVarTuple
from more_itertools import one
from more_itertools.more import filter_map


_C = TypeVar("_C", bound="FromString")
class FromString(Enum):
    @classmethod
    def _from_string(cls: Type[_C], val: str) -> _C:
        res = one(t for t in cls if t.value == val)
        print(res)
        return res


class Message(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True, use_enum_values=True)

    header: "MessageHeader"
    msg_id: str
    msg_type: "MessageType"
    parent_header: Optional["MessageHeader"]
    metadata: Dict
    content: "MessageContent"
    buffers: List
    # See https://jupyter-client.readthedocs.io/en/latest/messaging.html#request-reply
    status: Optional[Literal["busy", "idle", "ok", "error"]] = None

    @field_validator("msg_type", mode="before")
    @classmethod
    def parse_msg_type(cls, v: str) -> "MessageType":
        return MessageType.from_string(v)


class MessageType:
    @classmethod
    def from_string(cls: Type["MessageType"], val: str) -> "MessageType":
        if cls == MessageType:
            return one(
                variant
                for sub_cls in cls.__subclasses__()
                if sub_cls != MessageType
                and (variant := try_(sub_cls._from_string)(val=val)) is not None
            )
        elif issubclass(cls, FromString):
            return cls._from_string(val)
        else:
            raise ValueError()


class ShellChannelMessage(MessageType, FromString):
    ExecuteRequest = "execute_request"
    ExecuteReply = "execute_reply"
    InspectRequest = "inspect_request"
    InspecyReply = "inspect_reply"
    CompletedProquest = "complete_request"
    CompleteReply = "complete_reply"
    HistoryRequest = "history_request"
    HistoryReply = "history_reply"
    IsCompleteRequest = "is_complete_request"
    IsCompleteReply = "is_complete_reply"
    ConnectRequest = "connect_request"
    ConnectReply = "connect_reply"
    CommInfoRequest = "comm_info_request"
    CommInfoReply = "comm_info_reply"
    KernelInfoRequest = "kernel_info_request"
    KernelInfoReply = "kernel_info_reply"


class IoPubSubChannelMessage(MessageType, FromString):
    Stream = "stream"
    DisplayData = "display_data"
    UpdateDisplayData = "update_display_data"
    ExecuteInput = "execute_input"
    ExecuteResult = "execute_result"
    ExecutionError = "error"
    KernelStatus = "status"
    ClearOutput = "clear_output"
    DebugEvent = "debug_event"


class KernelStatus(Enum):
    Starting = "starting"
    Idle = "idle"
    Busy = "busy"


class MessageHeader(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True, use_enum_values=True)

    msg_id: str
    session: str
    username: str
    date: datetime
    msg_type: str
    version: str

    @field_serializer("date")
    def serialize_datetime(self, dt: datetime, _info):
        dt.isoformat()


class MessageContent(BaseModel):
    code: Optional[str] = None
    execution_count: Optional[int] = None
    execution_status: Optional[str] = None  # TODO: Enum for statuses
    name: Optional[Literal["stdout", "stderr"]] = None
    text: Optional[str] = None
    _other: Dict[str, Any] = {}

    @model_validator(mode="before")
    @classmethod
    def parse_msg(cls, data: Any):
        assert isinstance(data, Dict)
        data["_other"] = data.get("_other") or {}
        for key in list(k for k in data.keys() if k not in get_type_hints(cls)):
            data["_other"] = data.pop(key)

        return data


MessageHandler = Callable[[Message], None]


class KernelConnection:
    def __init__(self, kernel_id: str):
        self.kernel_id = kernel_id
        self._handlers: Dict[Type[MessageType], MessageHandler] = {}
        self._ksm = KernelSpecManager()
        self._km = AsyncKernelManager()
        self._executor = ThreadPoolExecutor(max_workers=4)
        self._client = None

    @property
    def client(self) -> AsyncKernelClient:
        if self._client is None:
            self._client = self._km.client(kernel_id=self.kernel_id)
        return self._client

    def spec_manager(self) -> KernelSpecManager:
        return self._ksm

    def execute_code(self, code: str):
        return self.client.execute(code=code)

    async def start_kernel(self):
        await self._km.start_kernel(kernel_id=self.kernel_id)
        self.client.start_channels()
        await self.client.wait_for_ready()
        print("Ready")

    async def shutdown(self):
        self.client.shutdown
        resp = await self.client.get_iopub_msg(timeout=1)
        print(f"{resp=}")
        sys.exit(0)

    def shutdown_blocking(self):
        self.client.shutdown
        resp = self.client.blocking_client().get_iopub_msg(timeout=1)
        print(f"{resp=}")
        sys.exit(0)

    async def listen(self):
        async def task():
            print("Starting listener task")
            while True:
                try:
                    msg = Message.validate(await self.client.get_iopub_msg())
                    if (handler := self._handlers.get(type(msg.msg_type), None)) is not None:
                        handler(msg)
                except Exception as e:
                    print(e)

        def callback(fut: Future):
            if (e := fut.exception()) is not None:
                print(f"Error: {e}")
            print(f"{fut.result()=}")

        return self._executor.submit(partial(asyncio.run, task()))

    def set_message_handler(self, msg_type: Type[MessageType], handler: MessageHandler, evict: bool = False):
        if msg_type in self._handlers and not evict:
            raise ValueError("Cannot reset handler without specifying `evict=True`")

        self._handlers[msg_type] = handler


_T = TypeVar("_T")
_P = ParamSpec("_P")
def try_(f: Callable[_P, _T], default: Optional[_T] = None) -> Callable[_P, Optional[_T]]:
    def wrapper(*args: _P.args, **kwargs: _P.kwargs) -> Optional[_T]:
        try:
            return f(*args, **kwargs)
        except Exception:
            return default

    return wrapper
