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

from message import Message, MessageType

try:
    import rich
except ModuleNotFoundError:
    logging.warning("Module `rich` not available, printing will not be pretty")

from typing_extensions import Annotated, ParamSpec, TypeVarTuple
from more_itertools import one
from more_itertools.more import filter_map


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
