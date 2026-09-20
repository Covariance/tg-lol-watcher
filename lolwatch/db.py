"""SQLite storage: schema, migrations and every query the app runs.

Times are UTC epoch milliseconds. One aiosqlite connection is shared; writes go
through `tx()`, which serialises them so concurrent coroutines can't commit each
other's half-finished transactions.
"""

from __future__ import annotations

import asyncio
import hashlib
from collections.abc import AsyncIterator, Iterable
from contextlib import asynccontextmanager

import aiosqlite

from .models import Match, Player, Session

SCHEMA_VERSION = 2

_SCHEMA_V1 = """
CREATE TABLE players (
  puuid          TEXT PRIMARY KEY,
  game_name      TEXT NOT NULL,
  tag_line       TEXT NOT NULL,
  platform       TEXT NOT NULL,
  active         INTEGER NOT NULL DEFAULT 1,
  added_by       INTEGER,
  added_at       INTEGER NOT NULL,
  last_seen_ms   INTEGER,
  name_checked_at INTEGER
);

CREATE TABLE matches (
  match_id       TEXT NOT NULL,
  puuid          TEXT NOT NULL,
  queue_id       INTEGER NOT NULL,
  champion       TEXT NOT NULL,
  win            INTEGER NOT NULL,        -- 1 win, 0 loss, -1 unknown
  remake         INTEGER NOT NULL DEFAULT 0,
  kills          INTEGER, deaths INTEGER, assists INTEGER,
  duration_s     INTEGER NOT NULL,
  start_ms       INTEGER NOT NULL,
  end_ms         INTEGER NOT NULL,
  ingested_at    INTEGER NOT NULL,
  PRIMARY KEY (match_id, puuid)
);
CREATE INDEX idx_matches_puuid_start ON matches(puuid, start_ms);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  puuid          TEXT NOT NULL,
  started_ms     INTEGER NOT NULL,
  last_end_ms    INTEGER NOT NULL,
  status         TEXT NOT NULL,           -- 'open' | 'flushed'
  flush_due_ms   INTEGER,
  postpones      INTEGER NOT NULL DEFAULT 0,
  notified_at    INTEGER
);
CREATE INDEX idx_sessions_open ON sessions(puuid, status);

CREATE TABLE session_matches (
  session_id     INTEGER NOT NULL,
  match_id       TEXT NOT NULL,
  puuid          TEXT NOT NULL,
  PRIMARY KEY (session_id, match_id, puuid)
);

CREATE TABLE subscribers (
  chat_id        INTEGER PRIMARY KEY,
  added_at       INTEGER NOT NULL
);

-- kv (alert dedupe, schema version, key fingerprint) is created by migrate() itself.
"""

_SCHEMA_V2 = """
CREATE TABLE allowed_users (
  user_id        INTEGER PRIMARY KEY,
  added_by       INTEGER,
  added_at       INTEGER NOT NULL
)
"""

# version -> script that upgrades from version-1. Append; never edit old entries.
MIGRATIONS: dict[int, str] = {1: _SCHEMA_V1, 2: _SCHEMA_V2}


def key_fingerprint(api_key: str) -> str:
    """Stable, non-reversible identifier for an API key (for change detection)."""
    return hashlib.sha256(api_key.encode()).hexdigest()[:16]


def _player(row: aiosqlite.Row) -> Player:
    return Player(
        puuid=row["puuid"], game_name=row["game_name"], tag_line=row["tag_line"],
        platform=row["platform"], active=bool(row["active"]), added_by=row["added_by"],
        added_at=row["added_at"], last_seen_ms=row["last_seen_ms"],
        name_checked_at=row["name_checked_at"],
    )


def _match(row: aiosqlite.Row) -> Match:
    return Match(
        match_id=row["match_id"], puuid=row["puuid"], queue_id=row["queue_id"],
        champion=row["champion"], win=row["win"], remake=bool(row["remake"]),
        kills=row["kills"], deaths=row["deaths"], assists=row["assists"],
        duration_s=row["duration_s"], start_ms=row["start_ms"], end_ms=row["end_ms"],
        ingested_at=row["ingested_at"],
    )


def _session(row: aiosqlite.Row) -> Session:
    return Session(
        id=row["id"], puuid=row["puuid"], started_ms=row["started_ms"],
        last_end_ms=row["last_end_ms"], status=row["status"],
        flush_due_ms=row["flush_due_ms"], postpones=row["postpones"],
        notified_at=row["notified_at"],
    )


class Database:
    def __init__(self, conn: aiosqlite.Connection):
        self._conn = conn
        self._lock = asyncio.Lock()

    @classmethod
    async def open(cls, path: str) -> Database:
        conn = await aiosqlite.connect(path)
        conn.row_factory = aiosqlite.Row
        await conn.execute("PRAGMA journal_mode=WAL")
        await conn.execute("PRAGMA busy_timeout=5000")
        db = cls(conn)
        await db.migrate()
        return db

    async def close(self) -> None:
        await self._conn.close()

    # -- transactions -------------------------------------------------------

    @asynccontextmanager
    async def tx(self) -> AsyncIterator[aiosqlite.Connection]:
        async with self._lock:
            try:
                yield self._conn
                await self._conn.commit()
            except BaseException:
                await self._conn.rollback()
                raise

    async def _fetchall(self, sql: str, args: Iterable = ()) -> list[aiosqlite.Row]:
        async with self._conn.execute(sql, tuple(args)) as cur:
            return list(await cur.fetchall())

    async def _fetchone(self, sql: str, args: Iterable = ()) -> aiosqlite.Row | None:
        async with self._conn.execute(sql, tuple(args)) as cur:
            return await cur.fetchone()

    # -- schema -------------------------------------------------------------

    async def migrate(self) -> None:
        async with self.tx() as conn:
            await conn.execute("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
            async with conn.execute("SELECT v FROM kv WHERE k='schema_version'") as cur:
                row = await cur.fetchone()
            current = int(row["v"]) if row else 0
            if current > SCHEMA_VERSION:
                raise RuntimeError(
                    f"database schema v{current} is newer than this code (v{SCHEMA_VERSION})")
            for version in range(current + 1, SCHEMA_VERSION + 1):
                script = MIGRATIONS[version]
                for stmt in filter(None, (s.strip() for s in script.split(";"))):
                    await conn.execute(stmt)
                await conn.execute(
                    "INSERT INTO kv(k, v) VALUES('schema_version', ?) "
                    "ON CONFLICT(k) DO UPDATE SET v=excluded.v", (str(version),))

    # -- kv -----------------------------------------------------------------

    async def kv_get(self, key: str) -> str | None:
        row = await self._fetchone("SELECT v FROM kv WHERE k=?", (key,))
        return row["v"] if row else None

    async def kv_set(self, key: str, value: str) -> None:
        async with self.tx() as conn:
            await conn.execute(
                "INSERT INTO kv(k, v) VALUES(?, ?) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                (key, value))

    # -- players ------------------------------------------------------------

    async def add_player(self, player: Player) -> None:
        """Insert, or reactivate + refresh names if the PUUID is already known."""
        async with self.tx() as conn:
            await conn.execute(
                "INSERT INTO players(puuid, game_name, tag_line, platform, active, added_by, "
                "added_at, last_seen_ms, name_checked_at) VALUES(?,?,?,?,1,?,?,?,?) "
                "ON CONFLICT(puuid) DO UPDATE SET active=1, game_name=excluded.game_name, "
                "tag_line=excluded.tag_line, platform=excluded.platform, "
                "name_checked_at=excluded.name_checked_at, "
                # A player coming back from /remove is new again: games played while
                # they were untracked must not be announced as fresh.
                "last_seen_ms=CASE WHEN players.active=0 THEN excluded.last_seen_ms "
                "ELSE players.last_seen_ms END",
                (player.puuid, player.game_name, player.tag_line, player.platform,
                 player.added_by, player.added_at, player.last_seen_ms,
                 player.name_checked_at))

    async def get_player(self, puuid: str) -> Player | None:
        row = await self._fetchone("SELECT * FROM players WHERE puuid=?", (puuid,))
        return _player(row) if row else None

    async def find_player(self, game_name: str, tag_line: str,
                          active_only: bool = True) -> Player | None:
        """Riot IDs are case-insensitive. Compared in Python: SQLite's NOCASE only
        folds ASCII, which would miss Cyrillic/Greek/etc. names."""
        wanted = (game_name.casefold(), tag_line.casefold())
        for p in await self.list_players(active_only):
            if (p.game_name.casefold(), p.tag_line.casefold()) == wanted:
                return p
        return None

    async def list_players(self, active_only: bool = True) -> list[Player]:
        sql = "SELECT * FROM players"
        if active_only:
            sql += " WHERE active=1"
        return [_player(r) for r in await self._fetchall(sql + " ORDER BY game_name COLLATE NOCASE")]

    async def deactivate_player(self, puuid: str) -> None:
        async with self.tx() as conn:
            await conn.execute("UPDATE players SET active=0 WHERE puuid=?", (puuid,))

    async def set_last_seen(self, puuid: str, last_seen_ms: int) -> None:
        async with self.tx() as conn:
            await conn.execute(
                "UPDATE players SET last_seen_ms=? WHERE puuid=? "
                "AND (last_seen_ms IS NULL OR last_seen_ms < ?)",
                (last_seen_ms, puuid, last_seen_ms))

    async def update_names(self, puuid: str, game_name: str, tag_line: str, checked_at: int) -> None:
        async with self.tx() as conn:
            await conn.execute(
                "UPDATE players SET game_name=?, tag_line=?, name_checked_at=? WHERE puuid=?",
                (game_name, tag_line, checked_at, puuid))

    async def rewrite_puuid(self, old: str, new: str) -> None:
        """Cascade a PUUID change through every table (see §4.4)."""
        if old == new:
            return
        async with self.tx() as conn:
            for table in ("players", "matches", "sessions", "session_matches"):
                await conn.execute(f"UPDATE {table} SET puuid=? WHERE puuid=?", (new, old))

    # -- matches ------------------------------------------------------------

    async def known_match_ids(self, puuid: str, match_ids: Iterable[str]) -> set[str]:
        ids = list(match_ids)
        if not ids:
            return set()
        marks = ",".join("?" * len(ids))
        rows = await self._fetchall(
            f"SELECT match_id FROM matches WHERE puuid=? AND match_id IN ({marks})",
            [puuid, *ids])
        return {r["match_id"] for r in rows}

    async def insert_match(self, m: Match) -> bool:
        """Returns True if inserted, False if it was already stored (idempotent)."""
        async with self.tx() as conn:
            cur = await conn.execute(
                "INSERT OR IGNORE INTO matches(match_id, puuid, queue_id, champion, win, remake, "
                "kills, deaths, assists, duration_s, start_ms, end_ms, ingested_at) "
                "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (m.match_id, m.puuid, m.queue_id, m.champion, m.win, int(m.remake), m.kills,
                 m.deaths, m.assists, m.duration_s, m.start_ms, m.end_ms, m.ingested_at))
            return cur.rowcount == 1

    async def delete_match(self, match_id: str, puuid: str) -> None:
        """Un-store a match so the next poll ingests it again."""
        async with self.tx() as conn:
            await conn.execute("DELETE FROM matches WHERE match_id=? AND puuid=?",
                               (match_id, puuid))

    async def matches_between(self, puuid: str, start_ms: int, end_ms: int) -> list[Match]:
        """Matches that *started* in [start_ms, end_ms), oldest first."""
        rows = await self._fetchall(
            "SELECT * FROM matches WHERE puuid=? AND start_ms>=? AND start_ms<? "
            "ORDER BY start_ms", (puuid, start_ms, end_ms))
        return [_match(r) for r in rows]

    async def last_game_ends(self) -> dict[str, int]:
        """puuid -> end time of the player's most recent stored game."""
        rows = await self._fetchall("SELECT puuid, MAX(end_ms) AS t FROM matches GROUP BY puuid")
        return {r["puuid"]: r["t"] for r in rows}

    async def session_matches(self, session_id: int) -> list[Match]:
        rows = await self._fetchall(
            "SELECT m.* FROM matches m JOIN session_matches sm "
            "ON sm.match_id=m.match_id AND sm.puuid=m.puuid "
            "WHERE sm.session_id=? ORDER BY m.start_ms", (session_id,))
        return [_match(r) for r in rows]

    # -- sessions -----------------------------------------------------------

    async def get_session(self, session_id: int) -> Session | None:
        row = await self._fetchone("SELECT * FROM sessions WHERE id=?", (session_id,))
        return _session(row) if row else None

    async def open_sessions(self, puuid: str | None = None) -> list[Session]:
        if puuid is None:
            rows = await self._fetchall("SELECT * FROM sessions WHERE status='open' ORDER BY id")
        else:
            rows = await self._fetchall(
                "SELECT * FROM sessions WHERE status='open' AND puuid=? "
                "ORDER BY last_end_ms DESC, id DESC", (puuid,))
        return [_session(r) for r in rows]

    async def create_session(self, puuid: str, started_ms: int, last_end_ms: int,
                             flush_due_ms: int) -> int:
        async with self.tx() as conn:
            cur = await conn.execute(
                "INSERT INTO sessions(puuid, started_ms, last_end_ms, status, flush_due_ms) "
                "VALUES(?,?,?,'open',?)", (puuid, started_ms, last_end_ms, flush_due_ms))
            return int(cur.lastrowid)

    async def update_session(self, session_id: int, *, started_ms: int, last_end_ms: int,
                             flush_due_ms: int | None, postpones: int | None = None) -> None:
        async with self.tx() as conn:
            if postpones is None:
                await conn.execute(
                    "UPDATE sessions SET started_ms=?, last_end_ms=?, flush_due_ms=? WHERE id=?",
                    (started_ms, last_end_ms, flush_due_ms, session_id))
            else:
                await conn.execute(
                    "UPDATE sessions SET started_ms=?, last_end_ms=?, flush_due_ms=?, "
                    "postpones=? WHERE id=?",
                    (started_ms, last_end_ms, flush_due_ms, postpones, session_id))

    async def set_postpone(self, session_id: int, postpones: int, flush_due_ms: int) -> None:
        async with self.tx() as conn:
            await conn.execute(
                "UPDATE sessions SET postpones=?, flush_due_ms=? WHERE id=?",
                (postpones, flush_due_ms, session_id))

    async def attach_match(self, session_id: int, match_id: str, puuid: str) -> None:
        async with self.tx() as conn:
            await conn.execute(
                "INSERT OR IGNORE INTO session_matches(session_id, match_id, puuid) VALUES(?,?,?)",
                (session_id, match_id, puuid))

    async def mark_flushed(self, session_id: int, notified_at: int) -> None:
        async with self.tx() as conn:
            await conn.execute(
                "UPDATE sessions SET status='flushed', notified_at=?, flush_due_ms=NULL WHERE id=?",
                (notified_at, session_id))

    # -- subscribers --------------------------------------------------------

    async def add_subscriber(self, chat_id: int, added_at: int) -> bool:
        async with self.tx() as conn:
            cur = await conn.execute(
                "INSERT OR IGNORE INTO subscribers(chat_id, added_at) VALUES(?,?)",
                (chat_id, added_at))
            return cur.rowcount == 1

    async def remove_subscriber(self, chat_id: int) -> bool:
        async with self.tx() as conn:
            cur = await conn.execute("DELETE FROM subscribers WHERE chat_id=?", (chat_id,))
            return cur.rowcount == 1

    async def is_subscriber(self, chat_id: int) -> bool:
        return await self._fetchone(
            "SELECT 1 FROM subscribers WHERE chat_id=?", (chat_id,)) is not None

    async def subscribers(self) -> list[int]:
        return [r["chat_id"] for r in await self._fetchall(
            "SELECT chat_id FROM subscribers ORDER BY chat_id")]

    # -- allowed users (who may talk to the bot at all) -----------------------

    async def add_allowed_user(self, user_id: int, added_by: int | None, added_at: int) -> bool:
        async with self.tx() as conn:
            cur = await conn.execute(
                "INSERT OR IGNORE INTO allowed_users(user_id, added_by, added_at) VALUES(?,?,?)",
                (user_id, added_by, added_at))
            return cur.rowcount == 1

    async def remove_allowed_user(self, user_id: int) -> bool:
        async with self.tx() as conn:
            cur = await conn.execute("DELETE FROM allowed_users WHERE user_id=?", (user_id,))
            return cur.rowcount == 1

    async def is_allowed_user(self, user_id: int) -> bool:
        return await self._fetchone(
            "SELECT 1 FROM allowed_users WHERE user_id=?", (user_id,)) is not None

    async def allowed_users(self) -> list[int]:
        return [r["user_id"] for r in await self._fetchall(
            "SELECT user_id FROM allowed_users ORDER BY user_id")]
