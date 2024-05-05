import asyncio
import logging
import signal
import sys
from datetime import datetime
from typing import Any, Dict, List, Literal, Optional, get_type_hints

try:
    import rich
except ModuleNotFoundError:
    logging.warning("Module `rich` not available, printing will not be pretty")

from jupyter_client.asynchronous.client import AsyncKernelClient
from jupyter_client.kernelspec import KernelSpecManager
from jupyter_client.manager import AsyncKernelManager
from pydantic import (
    BaseModel,
    ConfigDict,
    field_serializer,
    model_validator,
)
from pydantic.functional_validators import BeforeValidator
from traitlets.config import json
from typing_extensions import Annotated


class Message(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    header: "MessageHeader"
    msg_id: str
    msg_type: str
    parent_header: Optional["MessageHeader"]
    metadata: Dict
    content: "MessageContent"
    buffers: List


class MessageHeader(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

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
    execution_count: Optional[str] = None
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


async def listen(client: AsyncKernelClient):
    while True:
        msg = Message.validate(await client.get_iopub_msg())
        # rich.print(msg)
        if msg.msg_type == "stream" and msg.content.name == "stdout":
            print(msg.content.text)


async def main():
    ksm = KernelSpecManager()
    kernelspecs = ksm.get_all_specs()
    print(f"{kernelspecs=}")

    km = AsyncKernelManager()

    kernel_id = "python3"
    await km.start_kernel(kernel_id=kernel_id)
    client = km.client(kernel_id=kernel_id)
    client.start_channels()
    await client.wait_for_ready()

    listen_task = asyncio.create_task(coro=listen(client=client))
    code = "print('Hello World')"
    msg_id = client.execute(code)
    print(f"{msg_id=}")

    def shutdown(signal, frame):
        client.shutdown()
        resp = client.blocking_client().get_iopub_msg(timeout=1)
        print(f"{resp=}")
        sys.exit(0)

    signal.signal(signal.SIGINT, shutdown)
    while True:
        await listen(client)
        pass


if __name__ == "__main__":
    asyncio.run(main())
