"""Thin httpx-backed RiotClient. The only module that talks HTTP to Riot.

Five endpoints, called directly: account-v1, summoner-v4 (level only), match-v5
ids + match, spectator-v5. Match JSON goes straight to `parsing.parse_match`, so
the traps (duration units, remakes, Arena) stay in that one tested place.

Retries live in `retry.py`. This module only maps responses onto typed errors and
keeps us inside the application rate limits, so a backfill doesn't burn 429s.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import deque
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from typing import Any
from urllib.parse import quote

import httpx

from ..models import AccountInfo, MatchData
from .errors import (
    RiotAuthError,
    RiotBadRequest,
    RiotError,
    RiotNotFound,
    RiotRateLimited,
    RiotUnavailable,
)
from .parsing import MatchParseError, parse_match
from .routing import region_for

log = logging.getLogger(__name__)

HTTP_TIMEOUT_S = 30
# Dev and personal keys (spec §0.3): 20 requests/second and 100 per 2 minutes.
APP_RATE_LIMITS = ((20, 1.0), (100, 120.0))


def _account_region(platform: str) -> str:
    # account-v1 has no `sea` cluster; Riot serves those accounts from `asia`.
    region = region_for(platform)
    return "asia" if region == "sea" else region


def _split_match_id(match_id: str) -> tuple[str, int]:
    prefix, _, number = match_id.rpartition("_")
    if not prefix or not number.isdigit():
        raise ValueError(f"malformed match id {match_id!r}")
    return prefix.upper(), int(number)


def _retry_after(headers: httpx.Headers) -> float | None:
    try:
        return float(headers["Retry-After"])
    except (KeyError, ValueError):
        return None


def map_status(response: httpx.Response) -> RiotError | None:
    """Typed error for a failed response, or None for a success."""
    code = response.status_code
    if code < 400:
        return None
    what = f"HTTP {code} for {response.request.url.path}"
    if code in (401, 403):
        return RiotAuthError(what)
    if code == 404:
        return RiotNotFound(what)
    if code == 400:
        return RiotBadRequest(what)
    if code == 429:
        kind = response.headers.get("X-Rate-Limit-Type", "service")
        return RiotRateLimited(f"{what} ({kind} limit)",
                               retry_after=_retry_after(response.headers))
    if code >= 500:
        return RiotUnavailable(what)
    return RiotError(what)


class RateLimiter:
    """Client-side sliding windows over the application rate limits.

    Riot enforces the same windows server-side; staying under them here means a
    burst (a 100-match backfill) waits instead of collecting 429s. `pause` is how
    a 429 that does get through holds back every other caller too."""

    def __init__(self, limits: tuple[tuple[int, float], ...] = APP_RATE_LIMITS, *,
                 clock: Callable[[], float] = time.monotonic,
                 sleep: Callable[[float], Awaitable[None]] = asyncio.sleep):
        self._windows = [(n, per, deque[float]()) for n, per in limits]
        self._clock, self._sleep = clock, sleep
        self._paused_until = 0.0
        self._lock = asyncio.Lock()

    def pause(self, seconds: float) -> None:
        self._paused_until = max(self._paused_until, self._clock() + seconds)

    async def acquire(self) -> None:
        async with self._lock:
            while True:
                now = self._clock()
                wait = self._paused_until - now
                for n, per, stamps in self._windows:
                    while stamps and stamps[0] <= now - per:
                        stamps.popleft()
                    if len(stamps) >= n:
                        wait = max(wait, stamps[0] + per - now)
                if wait <= 0:
                    break
                await self._sleep(wait)
            for _, _, stamps in self._windows:
                stamps.append(now)


class HttpRiotClient:
    def __init__(self, api_key: str, *, transport: httpx.AsyncBaseTransport | None = None,
                 limiter: RateLimiter | None = None):
        self._http = httpx.AsyncClient(
            headers={"X-Riot-Token": api_key}, timeout=HTTP_TIMEOUT_S, transport=transport)
        self._limiter = limiter or RateLimiter()

    async def aclose(self) -> None:
        await self._http.aclose()

    async def _get(self, host: str, path: str, params: dict[str, Any] | None = None) -> Any:
        await self._limiter.acquire()
        try:
            response = await self._http.get(
                f"https://{host}.api.riotgames.com{path}", params=params)
        except httpx.TransportError as exc:      # timeouts, resets, DNS
            raise RiotUnavailable(f"{type(exc).__name__}: {exc}") from exc
        error = map_status(response)
        if error is not None:
            if isinstance(error, RiotRateLimited):
                self._limiter.pause(error.retry_after or 5.0)
            raise error
        try:
            return response.json()
        except ValueError as exc:
            raise RiotUnavailable(f"invalid JSON from {path}") from exc

    # -- accounts -----------------------------------------------------------

    async def _level(self, puuid: str, platform: str) -> int | None:
        try:
            data = await self._get(platform.lower(),
                                   f"/lol/summoner/v4/summoners/by-puuid/{puuid}")
            return data.get("summonerLevel")
        except RiotError as exc:   # level is decoration; never fail an /add over it
            log.debug("summoner level lookup failed: %s", exc)
            return None

    async def resolve_account(self, game_name: str, tag_line: str,
                              platform: str) -> AccountInfo:
        data = await self._get(
            _account_region(platform),
            f"/riot/account/v1/accounts/by-riot-id/{quote(game_name, safe='')}"
            f"/{quote(tag_line, safe='')}")
        puuid = data["puuid"]
        return AccountInfo(puuid=puuid, game_name=data.get("gameName") or game_name,
                           tag_line=data.get("tagLine") or tag_line,
                           summoner_level=await self._level(puuid, platform))

    async def account_by_puuid(self, puuid: str, platform: str) -> AccountInfo:
        data = await self._get(_account_region(platform),
                               f"/riot/account/v1/accounts/by-puuid/{puuid}")
        if not data.get("gameName") or not data.get("tagLine"):
            raise RiotNotFound(f"account {puuid[:12]}… has no Riot ID")
        return AccountInfo(puuid=puuid, game_name=data["gameName"], tag_line=data["tagLine"])

    # -- matches ------------------------------------------------------------

    async def recent_match_ids(self, puuid: str, platform: str,
                               since: datetime | None, count: int = 20) -> list[str]:
        params: dict[str, Any] = {"start": 0, "count": count}
        if since is not None:
            # match-v5 startTime is epoch *seconds*
            params["startTime"] = int(since.astimezone(UTC).timestamp())
        ids = await self._get(region_for(platform),
                              f"/lol/match/v5/matches/by-puuid/{puuid}/ids", params)
        return list(ids)

    async def get_match(self, match_id: str, platform: str) -> MatchData:
        prefix, _ = _split_match_id(match_id)   # the id, not the player, names the shard
        data = await self._get(region_for(prefix), f"/lol/match/v5/matches/{match_id}")
        info = data.get("info") if isinstance(data, dict) else None
        if not isinstance(info, dict):
            raise MatchParseError(f"match {match_id} has no info object")
        return parse_match(match_id, info)

    # -- spectator ----------------------------------------------------------

    async def is_in_game(self, puuid: str, platform: str) -> bool:
        try:
            await self._get(platform.lower(),
                            f"/lol/spectator/v5/active-games/by-summoner/{puuid}")
        except RiotNotFound:
            return False                 # 404 is the normal "not in a game" answer
        return True
