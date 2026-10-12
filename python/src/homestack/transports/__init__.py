"""Remote transport selection and lifecycle."""

from contextlib import contextmanager
from typing import Iterator

from ..config import Config
from ..models import AppError
from .base import Transport


@contextmanager
def open_transport(cfg: Config) -> Iterator[Transport]:
    if cfg.transport_type == "herdr":
        from .herdr import open_herdr_transport

        with open_herdr_transport(cfg) as transport:
            yield transport
        return
    raise AppError(
        f"Unsupported transport type {cfg.transport_type!r}; only 'herdr' is implemented"
    )


__all__ = ["Transport", "open_transport"]
