"""Shared fakes. Nothing here touches the network."""

from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

import pytest

from lolwatch.config import NotificationConfig
from lolwatch.db import Database
from lolwatch.flusher import SessionService
from lolwatch.models import AccountInfo, Match, MatchData, Player
from lolwatch.riot.errors import RiotError

FIXTURES = Path(__file__).parent / "fixtures"
T0 = 1_757_000_000_000     # arbitrary fixed instant (ms)
MIN = 60_000


def load_fixture(name: str) -> dict:
    return json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf8"))


class Clock:
    """Frozen, manually advanced clock returning epoch ms."""

    def __init__(self, now: int = T0):
        self.now = now

    def __call__(self) -> int:
        return self.now

    def set(self, now: int) -> None:
        self.now = now


class FakeScheduler:
    def __init__(self):
        self.calls: list[tuple[int, float]] = []

    def schedule(self, session_id: int, delay_s: float) -> None:
        self.calls.append((session_id, delay_s))

    @property
    def last(self) -> tuple[int, float]:
        return self.calls[-1]


class FakeNotifier:
    def __init__(self):
        self.sessions: list[tuple[Player, list[Match]]] = []
        self.games: list[tuple[Player, Match]] = []
        self.ok = True

    async def send_session(self, player, matches):
        if not self.ok:
            return False
        self.sessions.append((player, matches))
        return True

    async def send_game(self, player, match):
        if not self.ok:
            return False
        self.games.append((player, match))
        return True


class FakeRiot:
    """In-memory RiotClient. `in_game` may be a bool or an exception to raise."""

    def __init__(self):
        self.in_game: bool | RiotError = False
        self.in_game_calls = 0
        self.ids: dict[str, list[str]] = {}
        self.matches: dict[str, MatchData] = {}
        self.get_match_calls: list[str] = []
        self.id_calls: list[tuple[str, datetime | None, int]] = []
        self.accounts: dict[tuple[str, str], AccountInfo] = {}
        self.closed = False
        self.by_puuid: dict[str, AccountInfo] = {}
        self.error: Exception | None = None

    async def resolve_account(self, game_name, tag_line, platform):
        if self.error:
            raise self.error
        return self.accounts[(game_name.lower(), tag_line.lower())]

    async def account_by_puuid(self, puuid, platform):
        return self.by_puuid[puuid]

    async def recent_match_ids(self, puuid, platform, since, count=20):
        if self.error:
            raise self.error
        self.id_calls.append((puuid, since, count))
        return list(self.ids.get(puuid, []))

    async def get_match(self, match_id, platform):
        self.get_match_calls.append(match_id)
        return self.matches[match_id]

    async def is_in_game(self, puuid, platform):
        self.in_game_calls += 1
        if isinstance(self.in_game, Exception):
            raise self.in_game
        return self.in_game

    async def aclose(self):
        self.closed = True


def make_player(puuid: str = "puuid-alice", name: str = "Alice", tag: str = "EUW",
                last_seen_ms: int | None = None, **kw) -> Player:
    return Player(puuid=puuid, game_name=name, tag_line=tag, platform="euw1",
                  added_at=T0, last_seen_ms=last_seen_ms, **kw)


def make_match(match_id: str, start_ms: int, minutes: int = 30, *, puuid: str = "puuid-alice",
               queue_id: int = 420, champion: str = "Azir", win: int = 1,
               remake: bool = False, kda: tuple[int, int, int] = (5, 2, 7)) -> Match:
    return Match(match_id=match_id, puuid=puuid, queue_id=queue_id, champion=champion,
                 win=win, remake=remake, kills=kda[0], deaths=kda[1], assists=kda[2],
                 duration_s=minutes * 60, start_ms=start_ms, end_ms=start_ms + minutes * MIN,
                 ingested_at=T0)


@pytest.fixture
async def db(tmp_path):
    database = await Database.open(str(tmp_path / "test.db"))
    yield database
    await database.close()


class Env:
    """A wired SessionService over a real (temp-file) SQLite DB and fakes."""

    def __init__(self, db: Database, cfg: NotificationConfig | None = None):
        self.db = db
        self.clock = Clock()
        self.riot = FakeRiot()
        self.notifier = FakeNotifier()
        self.scheduler = FakeScheduler()
        self.cfg = cfg or NotificationConfig()
        self.service = self.make_service()

    def make_service(self, scheduler: FakeScheduler | None = None) -> SessionService:
        return SessionService(self.db, self.riot, self.notifier, scheduler or self.scheduler,
                              self.cfg, clock=self.clock)

    async def add_player(self, player: Player | None = None) -> Player:
        player = player or make_player()
        await self.db.add_player(player)
        return player

    async def ingest(self, player: Player, match: Match) -> None:
        """Mimic the poller: store, then hand to the session service."""
        assert await self.db.insert_match(match)
        await self.service.on_match(player, match)


@pytest.fixture
async def env(db):
    e = Env(db)
    await e.add_player()
    return e
