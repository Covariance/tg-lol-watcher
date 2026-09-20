"""The object graph handed to Telegram handlers and jobs."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass

from .config import Config
from .db import Database
from .flusher import SessionService
from .notifier import Alerter, Notifier
from .poller import Poller
from .riot.adapter import RiotClient


@dataclass
class Services:
    cfg: Config
    db: Database
    riot: RiotClient
    poller: Poller
    sessions: SessionService
    notifier: Notifier
    alerter: Alerter
    # Returns the next scheduled weekly run (aware datetime) or None; set by the app.
    next_weekly: Callable[[], object | None] = lambda: None
