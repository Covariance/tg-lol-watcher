"""Cassiopeia-backed RiotClient. The only module that imports `cassiopeia`.

Cassiopeia is synchronous, so every call runs in a worker thread. Calls are also
serialised behind one lock: its pipeline/rate limiters are process-global and we
have nothing to gain from concurrency (spec §0.3).

We deliberately configure a *bare* pipeline (RiotAPI only — no Cache, no Data
Dragon): the cache would serve stale spectator results, and Data Dragon would
add static-data downloads for champion names we already get from match JSON.

Match parsing is NOT done here. We rebuild a match-v5 `info` dict from
Cassiopeia's internal core data and hand it to `parsing.parse_match`, so the
traps (duration units, remakes, Arena) live in one tested, transport-agnostic
place. That rebuild reaches into `_data`, which is why it is isolated in
`core_to_info` and covered by an offline test.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import logging
from collections.abc import Callable
from datetime import UTC, datetime
from typing import Any, TypeVar

import cassiopeia as cass
import requests
from cassiopeia.core.match import MatchData as CoreMatchData
from cassiopeia.data import Platform
from cassiopeia.datastores import common as _cass_common
from cassiopeia.datastores.riotapi import account as _cass_account

from ..models import AccountInfo, MatchData
from .errors import (
    RiotAuthError,
    RiotBadRequest,
    RiotError,
    RiotNotFound,
    RiotRateLimited,
    RiotUnavailable,
)
from .parsing import parse_match

log = logging.getLogger(__name__)
T = TypeVar("T")


def map_exception(exc: BaseException) -> RiotError | None:
    """Translate a Cassiopeia/HTTP failure to a typed error, or None if unknown.

    Cassiopeia's class names are misleading (403 becomes `APIRequestError`, 401
    becomes `APIForbiddenError`, an exhausted 429 becomes a bare RuntimeError),
    so this keys off the numeric status code instead.
    """
    if isinstance(exc, RiotError):
        return exc
    code = getattr(exc, "code", None)
    if code is None and isinstance(exc, RuntimeError) and "429" in str(exc):
        code = 429
    if code in (401, 403):
        return RiotAuthError(f"HTTP {code}: {exc}")
    if code == 404:
        return RiotNotFound(str(exc))
    if code == 400:
        return RiotBadRequest(str(exc))
    if code == 429:
        headers = getattr(exc, "response_headers", None) or {}
        try:
            retry_after = float(headers.get("Retry-After"))
        except (TypeError, ValueError):
            retry_after = None
        return RiotRateLimited(str(exc), retry_after=retry_after)
    if isinstance(code, int) and code >= 500:
        return RiotUnavailable(f"HTTP {code}: {exc}")
    # datapipelines.NotFoundError carries no code
    if type(exc).__name__ == "NotFoundError":
        return RiotNotFound(str(exc))
    if isinstance(exc, (TimeoutError, ConnectionError, OSError)):
        return RiotUnavailable(f"{type(exc).__name__}: {exc}")
    if "timeout" in str(exc).lower():
        return RiotUnavailable(str(exc))
    return None


def _split_match_id(match_id: str) -> tuple[str, int]:
    prefix, _, number = match_id.rpartition("_")
    if not prefix or not number.isdigit():
        raise ValueError(f"malformed match id {match_id!r}")
    return prefix.upper(), int(number)


def core_to_info(md: Any) -> dict[str, Any]:
    """Rebuild a match-v5 `info` dict from Cassiopeia's `MatchData` core object."""
    participants = []
    for p in getattr(md, "participants", None) or []:
        stats = getattr(p, "stats", None)
        participants.append({
            "puuid": getattr(p, "puuid", None),
            "championName": getattr(p, "championName", None),
            "championId": getattr(p, "championId", None),
            "win": getattr(stats, "win", None),
            "kills": getattr(stats, "kills", None),
            "deaths": getattr(stats, "deaths", None),
            "assists": getattr(stats, "assists", None),
            "gameEndedInEarlySurrender": getattr(p, "endedInEarlySurrender", None),
        })
    return {
        "gameCreation": getattr(md, "gameCreation", None),
        "gameStartTimestamp": getattr(md, "gameStartTimestamp", None),
        "gameEndTimestamp": getattr(md, "gameEndTimestamp", None),
        "gameDuration": getattr(md, "gameDuration", None),
        "queueId": getattr(md, "queue", None),
        "participants": participants,
    }


def _patch_account_json() -> None:
    """Cassiopeia 5.2.0's account endpoint uses `data = self._get(...)` and then
    `data["region"] = ...`, i.e. it expects parsed JSON. Every other endpoint does
    `json.loads(self._get(...))` instead. The `requests` fallback (used when pycurl is
    absent, e.g. on Windows) only parses JSON when Content-Type carries a charset,
    which Riot doesn't send, so the account call gets a `str` and fails with
    "type AccountDto is not supported". Parse for that endpoint only; a global fix
    would break the endpoints that parse for themselves."""
    if getattr(_cass_account.AccountAPI._get, "_lolwatch", False):
        return
    original = _cass_account.AccountAPI._get

    def _get(self, *args, **kwargs):
        body = original(self, *args, **kwargs)
        return json.loads(body) if isinstance(body, (str, bytes)) else body

    _get._lolwatch = True  # type: ignore[attr-defined]
    _cass_account.AccountAPI._get = _get


_patch_account_json()

HTTP_TIMEOUT_S = 30


class _TimeoutRequests:
    """`requests` with a default timeout. Cassiopeia's fallback client calls
    `requests.get(url, headers=...)` with none, so one hung connection would block
    its worker thread, and with it our global call lock, forever."""

    def __getattr__(self, name: str) -> Any:
        return getattr(requests, name)

    def get(self, url: str, **kwargs: Any) -> requests.Response:
        kwargs.setdefault("timeout", HTTP_TIMEOUT_S)
        return requests.get(url, **kwargs)


def _patch_http_timeout() -> None:
    if not _cass_common.USE_PYCURL and not isinstance(_cass_common.requests, _TimeoutRequests):
        _cass_common.requests = _TimeoutRequests()


_patch_http_timeout()


class CassiopeiaClient:
    def __init__(self, api_key: str):
        settings = cass.get_default_config()
        settings["pipeline"] = {"RiotAPI": {"api_key": api_key}}
        settings["logging"] = {"print_calls": False, "print_riot_api_key": False,
                               "default": "WARNING", "core": "WARNING"}
        cass.apply_settings(settings)
        self._lock = asyncio.Lock()

    async def _run(self, fn: Callable[[], T]) -> T:
        async with self._lock:
            try:
                return await asyncio.to_thread(fn)
            except BaseException as exc:
                mapped = map_exception(exc)
                if mapped is None or mapped is exc:
                    raise
                raise mapped from exc

    # -- accounts -----------------------------------------------------------

    @staticmethod
    def _account_info(account: Any, level: int | None) -> AccountInfo:
        return AccountInfo(puuid=account.puuid, game_name=account.name,
                           tag_line=account.tagline, summoner_level=level)

    @staticmethod
    def _level(puuid: str, platform: Platform) -> int | None:
        try:
            return cass.get_summoner(puuid=puuid, region=platform.region).level
        except Exception as exc:  # level is decoration; never fail an /add over it
            log.debug("summoner level lookup failed: %s", exc)
            return None

    async def resolve_account(self, game_name: str, tag_line: str,
                              platform: str) -> AccountInfo:
        plat = Platform(platform.upper())

        def call() -> AccountInfo:
            account = cass.get_account(name=game_name, tagline=tag_line, region=plat.region)
            puuid = account.puuid  # forces the load; raises on unknown Riot ID
            return self._account_info(account, self._level(puuid, plat))

        return await self._run(call)

    async def account_by_puuid(self, puuid: str, platform: str) -> AccountInfo:
        plat = Platform(platform.upper())

        def call() -> AccountInfo:
            account = cass.get_account(puuid=puuid, region=plat.region)
            _ = account.name  # forces the load
            return self._account_info(account, None)

        return await self._run(call)

    # -- matches ------------------------------------------------------------

    async def recent_match_ids(self, puuid: str, platform: str,
                               since: datetime | None, count: int = 20) -> list[str]:
        plat = Platform(platform.upper())
        start_time = None
        if since is not None:
            # match-v5 startTime is epoch *seconds*
            start_time = int(since.astimezone(UTC).timestamp())

        def call() -> list[str]:
            history = cass.get_match_history(
                continent=plat.continent, puuid=puuid, start_time=start_time,
                start=0, count=count)
            # The history is a lazy paged list; islice stops it after one page.
            return [f"{plat.value}_{m.id}" for m in itertools.islice(history, count)]

        return await self._run(call)

    async def get_match(self, match_id: str, platform: str) -> MatchData:
        prefix, number = _split_match_id(match_id)
        plat = Platform(prefix)
        if plat.value.lower() != platform.lower():
            log.debug("match %s fetched via platform %s", match_id, platform)

        def call() -> MatchData:
            match = cass.get_match(id=number, region=plat.region)
            match.load()
            return parse_match(match_id, core_to_info(match._data[CoreMatchData]))

        return await self._run(call)

    # -- spectator ----------------------------------------------------------

    async def is_in_game(self, puuid: str, platform: str) -> bool:
        plat = Platform(platform.upper())

        def call() -> bool:
            current = cass.get_current_match(
                summoner=cass.get_summoner(puuid=puuid, region=plat.region),
                region=plat.region)
            return bool(current.exists)  # 404 -> False; auth/5xx errors propagate

        return await self._run(call)
