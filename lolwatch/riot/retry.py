"""Retry policy wrapper: transient failures are handled here, not in callers."""

from __future__ import annotations

import asyncio
import logging
import random
from collections.abc import Awaitable, Callable
from datetime import datetime
from typing import TypeVar

from ..models import AccountInfo, MatchData
from .adapter import RiotClient
from .errors import RiotRateLimited, RiotUnavailable

log = logging.getLogger(__name__)
T = TypeVar("T")

MAX_RATE_LIMIT_WAIT_S = 130.0  # one full 2-minute application window


class RetryingClient:
    """Wraps any RiotClient. `RiotUnavailable` retries with jittered backoff;
    `RiotRateLimited` waits `retry_after` once. Auth/NotFound/BadRequest pass through."""

    def __init__(self, inner: RiotClient, *, attempts: int = 3, base_delay: float = 2.0,
                 sleep: Callable[[float], Awaitable[None]] = asyncio.sleep):
        self._inner = inner
        self._attempts = attempts
        self._base = base_delay
        self._sleep = sleep

    async def _call(self, name: str, fn: Callable[[], Awaitable[T]]) -> T:
        rate_limit_retried = False
        attempt = 0
        while True:
            try:
                return await fn()
            except RiotRateLimited as exc:
                if rate_limit_retried:
                    raise
                rate_limit_retried = True
                wait = min(exc.retry_after or 5.0, MAX_RATE_LIMIT_WAIT_S)
                log.warning("riot %s rate limited; waiting %.1fs", name, wait)
                await self._sleep(wait)
            except RiotUnavailable as exc:
                attempt += 1
                if attempt >= self._attempts:
                    raise
                delay = self._base * (2 ** (attempt - 1)) * random.uniform(0.5, 1.5)
                log.warning("riot %s unavailable (%s); retry %d/%d in %.1fs",
                            name, exc, attempt, self._attempts - 1, delay)
                await self._sleep(delay)

    async def resolve_account(self, game_name: str, tag_line: str, platform: str) -> AccountInfo:
        return await self._call("resolve_account",
                                lambda: self._inner.resolve_account(game_name, tag_line, platform))

    async def account_by_puuid(self, puuid: str, platform: str) -> AccountInfo:
        return await self._call("account_by_puuid",
                                lambda: self._inner.account_by_puuid(puuid, platform))

    async def recent_match_ids(self, puuid: str, platform: str, since: datetime | None,
                               count: int = 20) -> list[str]:
        return await self._call("recent_match_ids",
                                lambda: self._inner.recent_match_ids(puuid, platform, since, count))

    async def get_match(self, match_id: str, platform: str) -> MatchData:
        return await self._call("get_match", lambda: self._inner.get_match(match_id, platform))

    async def is_in_game(self, puuid: str, platform: str) -> bool:
        return await self._call("is_in_game", lambda: self._inner.is_in_game(puuid, platform))
