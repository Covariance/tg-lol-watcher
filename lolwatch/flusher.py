"""I/O shell around `sessions.py`: assigns ingested matches to sessions, runs the
flush check, and recovers open sessions after a restart."""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Callable
from typing import Protocol

from .config import NotificationConfig
from .db import Database
from .models import Match, Player, Session
from .riot.adapter import RiotClient
from .riot.errors import RiotError
from .sessions import assign, decide_flush, flush_delay_s

log = logging.getLogger(__name__)


def now_ms() -> int:
    return int(time.time() * 1000)


class FlushScheduler(Protocol):
    def schedule(self, session_id: int, delay_s: float) -> None:
        """(Re)schedule the flush check for a session, replacing any pending one."""
        ...


class SessionNotifier(Protocol):
    async def send_session(self, player: Player, matches: list[Match]) -> bool: ...
    async def send_game(self, player: Player, match: Match) -> bool: ...


class SessionService:
    def __init__(self, db: Database, riot: RiotClient, notifier: SessionNotifier,
                 scheduler: FlushScheduler, cfg: NotificationConfig,
                 clock: Callable[[], int] = now_ms):
        self._db, self._riot, self._notifier = db, riot, notifier
        self._scheduler, self._cfg, self._clock = scheduler, cfg, clock
        self._lock = asyncio.Lock()   # serialises assignment vs. flush decisions

    # -- ingestion ----------------------------------------------------------

    async def on_match(self, player: Player, match: Match) -> None:
        """Called once for each *newly ingested* match, oldest first."""
        mode = self._cfg.mode
        if mode == "off":
            log.debug("notify decision: mode=off match=%s player=%s -> skip",
                      match.match_id, player.riot_id)
            return
        if mode == "per_game":
            ok = await self._notifier.send_game(player, match)
            log.debug("notify decision: mode=per_game match=%s player=%s sent=%s",
                      match.match_id, player.riot_id, ok)
            return

        async with self._lock:
            open_sessions = await self._db.open_sessions(player.puuid)
            open_session = open_sessions[0] if open_sessions else None
            a = assign(open_session, match.start_ms, match.end_ms, self._cfg.gap_ms)

            if a.flush_open and open_session is not None:
                log.debug("session=%d stale (new match %s starts after gap); flushing now",
                          open_session.id, match.match_id)
                await self._flush_now(open_session, reason="superseded by a later game")

            if a.attach and open_session is not None:
                session_id = open_session.id
                await self._db.update_session(
                    session_id, started_ms=a.started_ms, last_end_ms=a.last_end_ms,
                    flush_due_ms=a.flush_due_ms, postpones=0)
                log.debug("session=%d attach match=%s last_end=%d flush_due=%d",
                          session_id, match.match_id, a.last_end_ms, a.flush_due_ms)
            else:
                session_id = await self._db.create_session(
                    player.puuid, a.started_ms, a.last_end_ms, a.flush_due_ms)
                log.debug("session=%d created for match=%s player=%s flush_due=%d",
                          session_id, match.match_id, player.riot_id, a.flush_due_ms)
            await self._db.attach_match(session_id, match.match_id, player.puuid)
            self._scheduler.schedule(
                session_id, max(0.0, (a.flush_due_ms - self._clock()) / 1000))

    # -- flush job ----------------------------------------------------------

    def retry_flush_soon(self, session_id: int) -> None:
        """Re-arm the flush check after the job itself crashed."""
        self._scheduler.schedule(session_id, self._cfg.recheck_ms / 1000)

    async def flush(self, session_id: int) -> None:
        """Job callback: is the session over? If the player is still in a game,
        postpone; otherwise send the summary."""
        session = await self._db.get_session(session_id)
        if session is None or session.status != "open":
            log.debug("session=%d flush job: already flushed/missing, ignoring", session_id)
            return
        player = await self._db.get_player(session.puuid)
        if player is None:
            log.warning("session=%d belongs to unknown player; closing silently", session_id)
            await self._db.mark_flushed(session_id, self._clock())
            return

        # Network call outside the lock so the poller isn't blocked behind it.
        in_game: bool | None
        try:
            in_game = await self._riot.is_in_game(player.puuid, player.platform)
        except RiotError as exc:
            log.info("session=%d spectator lookup failed (%s: %s)",
                     session_id, type(exc).__name__, exc)
            in_game = None

        async with self._lock:
            # Re-read: a new match may have attached while we were waiting on Riot.
            session = await self._db.get_session(session_id)
            if session is None or session.status != "open":
                return
            now = self._clock()
            if session.flush_due_ms is not None and session.flush_due_ms > now:
                remaining = (session.flush_due_ms - now) / 1000
                log.debug("session=%d not due yet (%.0fs left: extended while checking "
                          "spectator, or job fired early); rescheduling", session_id, remaining)
                self._scheduler.schedule(session_id, remaining)
                return
            decision = decide_flush(
                postpones=session.postpones, in_game=in_game,
                max_postpones=self._cfg.max_postpones, now_ms=now,
                recheck_ms=self._cfg.recheck_ms)
            log.debug("session=%d flush decision: flush=%s (%s)",
                      session_id, decision.flush, decision.reason)
            if not decision.flush:
                assert decision.flush_due_ms is not None
                await self._db.set_postpone(session_id, decision.postpones,
                                            decision.flush_due_ms)
                self._scheduler.schedule(session_id, self._cfg.recheck_ms / 1000)
                return
            await self._flush_now(session, reason=decision.reason)

    async def _flush_now(self, session: Session, *, reason: str) -> None:
        """Send the summary and mark the session flushed. Caller holds the lock."""
        player = await self._db.get_player(session.puuid)
        matches = await self._db.session_matches(session.id)
        if player is None or not matches:
            log.warning("session=%d has no player/matches; closing silently", session.id)
            await self._db.mark_flushed(session.id, self._clock())
            return
        try:
            sent = await self._notifier.send_session(player, matches)
        except Exception:
            log.exception("session=%d notification raised", session.id)
            sent = False
        if not sent and session.postpones < self._cfg.max_postpones:
            # Delivery failed: retry later rather than losing the notification.
            postpones = session.postpones + 1
            await self._db.set_postpone(session.id, postpones,
                                        self._clock() + self._cfg.recheck_ms)
            self._scheduler.schedule(session.id, self._cfg.recheck_ms / 1000)
            log.warning("session=%d delivery failed; retry %d/%d in %dm", session.id,
                        postpones, self._cfg.max_postpones, self._cfg.flush_recheck_minutes)
            return
        await self._db.mark_flushed(session.id, self._clock())
        log.info("session=%d flushed (%s) player=%s games=%d delivered=%s",
                 session.id, reason, player.riot_id, len(matches), sent)

    # -- restart recovery ---------------------------------------------------

    async def recover(self) -> int:
        """Reschedule every open session. Overdue ones run immediately."""
        sessions = await self._db.open_sessions()
        now = self._clock()
        for s in sessions:
            delay = flush_delay_s(s, now)
            log.info("session=%d recovered: due in %.0fs", s.id, delay)
            self._scheduler.schedule(s.id, delay)
        return len(sessions)
