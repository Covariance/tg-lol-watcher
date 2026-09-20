"""Periodic match-history scan and idempotent ingestion."""

from __future__ import annotations

import logging
from collections.abc import Callable
from datetime import UTC, datetime

from .db import Database
from .flusher import SessionService, now_ms
from .models import Match, MatchData, Player
from .notifier import Alerter
from .riot.adapter import RiotClient
from .riot.errors import RiotAuthError, RiotBadRequest, RiotError, RiotNotFound, RiotRateLimited
from .riot.parsing import MAX_GAME_S, MatchParseError

log = logging.getLogger(__name__)

DAY_MS = 86_400_000
# Re-scan this far behind last_seen. Riot's startTime semantics (start vs end of
# game) and match-history lag both make an exact cut-off fragile; ingestion is
# idempotent and known ids cost no API call, so overlapping is free.
OVERLAP_MS = MAX_GAME_S * 1000
INITIAL_BACKFILL_DAYS = 7
NAME_REFRESH_MS = 7 * DAY_MS
# A match Riot lists but can't serve yet (404) is retried soon: the details lag the list.
# One that is unparseable or doesn't include the player is deterministic: retry rarely.
NOT_FOUND_RETRY_MS = 30 * 60_000
UNUSABLE_RETRY_MS = DAY_MS

AUTH_ALERT = ("Riot API rejected the key (403). The key is probably expired — replace "
              "RIOT_API_KEY and restart. Polling is paused until it works.")


def _dt(ms: int) -> datetime:
    return datetime.fromtimestamp(ms / 1000, tz=UTC)


class Poller:
    def __init__(self, db: Database, riot: RiotClient, sessions: SessionService | None,
                 alerter: Alerter | None, clock: Callable[[], int] = now_ms):
        self._db, self._riot, self._sessions = db, riot, sessions
        self._alerter, self._clock = alerter, clock
        # (match_id, puuid) we can't use right now -> when to try again (epoch ms)
        self._bad_matches: dict[tuple[str, str], int] = {}

    # -- the scheduled job --------------------------------------------------

    async def poll_once(self) -> int:
        """Scan every active player. Never raises: one bad player or a Riot
        hiccup must not kill the loop. Returns the number of new matches."""
        total = 0
        cache: dict[str, MatchData] = {}
        try:
            players = await self._db.list_players()
        except Exception:
            log.exception("poll: could not list players")
            return 0
        for player in players:
            try:
                total += await self.ingest(player, cache=cache)
            except RiotAuthError as exc:
                log.error("poll: Riot auth failure: %s", exc)
                if self._alerter:
                    await self._alerter.alert("riot_auth", AUTH_ALERT)
                break                       # every remaining call would fail too
            except RiotRateLimited as exc:
                log.warning("poll: rate limited (%s); ending this cycle early", exc)
                break
            except RiotBadRequest as exc:
                log.error("poll: Riot rejected the request for %s (%s). If the API key "
                          "was recently changed, PUUIDs are stale: run "
                          "`uv run python -m lolwatch.cli repuuid`", player.riot_id, exc)
            except RiotError as exc:
                log.warning("poll: %s for %s: %s", type(exc).__name__, player.riot_id, exc)
            except Exception as exc:
                log.exception("poll: unexpected failure for %s", player.riot_id)
                if self._alerter:
                    await self._alerter.alert(
                        "poll_error", f"Poller error for {player.riot_id}: "
                                      f"{type(exc).__name__}: {exc}. Still running.")
        if total:
            log.info("poll: ingested %d new match(es) for %d player(s)", total, len(players))
        return total

    # -- ingestion ----------------------------------------------------------

    async def ingest(self, player: Player, *, cache: dict[str, MatchData] | None = None,
                     since_ms: int | None = None, notify: bool | None = None,
                     count: int = 20) -> int:
        """Fetch and store matches we haven't seen. Returns how many were new.

        A player with no `last_seen_ms` is being seen for the first time: their
        recent history is stored *silently* (no session/notification for games
        that ended before we started watching).
        """
        cache = {} if cache is None else cache
        first_time = player.last_seen_ms is None
        started_at = self._clock()
        if notify is None:
            notify = not first_time
        if since_ms is None:
            since_ms = (started_at - INITIAL_BACKFILL_DAYS * DAY_MS if first_time
                        else player.last_seen_ms - OVERLAP_MS)

        ids = await self._riot.recent_match_ids(
            player.puuid, player.platform, _dt(since_ms), count)
        if len(ids) >= count:
            log.warning("ingest: %s returned %d ids (the page limit); older games in the "
                        "window may be missing", player.riot_id, len(ids))
        known = await self._db.known_match_ids(player.puuid, ids)
        fresh = [i for i in ids if i not in known and not self._is_bad((i, player.puuid))]

        rows: list[Match] = []
        for match_id in fresh:
            row = await self._load(player, match_id, cache)
            if row is not None:
                rows.append(row)
        rows.sort(key=lambda m: m.start_ms)   # oldest first: sessions build in order

        inserted = 0
        for row in rows:
            if not await self._db.insert_match(row):
                continue                       # raced with another writer; already counted
            inserted += 1
            log.debug("ingest: stored match=%s player=%s queue=%d start=%d notify=%s",
                      row.match_id, player.riot_id, row.queue_id, row.start_ms, notify)
            if notify and self._sessions is not None:
                try:
                    await self._sessions.on_match(player, row)
                except Exception:
                    # Stored but never announced would mean it is "known" forever and the
                    # notification lost. Un-store it so the next poll tries again.
                    await self._db.delete_match(row.match_id, row.puuid)
                    raise

        newest = max((r.start_ms for r in rows), default=0)
        marker = max(newest, started_at) if first_time else newest
        if marker:
            await self._db.set_last_seen(player.puuid, marker)
        return inserted

    def _is_bad(self, key: tuple[str, str]) -> bool:
        retry_at = self._bad_matches.get(key)
        if retry_at is None:
            return False
        if self._clock() >= retry_at:
            del self._bad_matches[key]
            return False
        return True

    def _mark_bad(self, key: tuple[str, str], retry_after_ms: int) -> None:
        now = self._clock()
        if len(self._bad_matches) > 1000:      # bounded: drop what has expired
            self._bad_matches = {k: t for k, t in self._bad_matches.items() if t > now}
        self._bad_matches[key] = now + retry_after_ms

    async def _load(self, player: Player, match_id: str,
                    cache: dict[str, MatchData]) -> Match | None:
        key = (match_id, player.puuid)
        data = cache.get(match_id)
        if data is None:
            try:
                data = await self._riot.get_match(match_id, player.platform)
            except RiotNotFound:
                log.warning("ingest: match %s not found (yet?); will retry later", match_id)
                self._mark_bad(key, NOT_FOUND_RETRY_MS)
                return None
            except MatchParseError as exc:
                log.warning("ingest: match %s unparseable (%s); skipping", match_id, exc)
                self._mark_bad(key, UNUSABLE_RETRY_MS)
                return None
            cache[match_id] = data
        part = data.participants.get(player.puuid)
        if part is None:
            log.warning("ingest: %s not among participants of %s; skipping",
                        player.riot_id, match_id)
            self._mark_bad(key, UNUSABLE_RETRY_MS)
            return None
        return Match(
            match_id=match_id, puuid=player.puuid, queue_id=data.queue_id,
            champion=part.champion, win=part.win, remake=data.remake,
            kills=part.kills, deaths=part.deaths, assists=part.assists,
            duration_s=data.duration_s, start_ms=data.start_ms, end_ms=data.end_ms,
            ingested_at=self._clock())

    async def backfill(self, player: Player, days: int) -> int:
        """Silently store the last `days` of history (no notifications)."""
        return await self.ingest(player, since_ms=self._clock() - days * DAY_MS,
                                 notify=False, count=100)

    # -- names --------------------------------------------------------------

    async def refresh_names(self) -> int:
        """Re-resolve display names older than a week. The PUUID is the identity."""
        now = self._clock()
        changed = 0
        for player in await self._db.list_players():
            if player.name_checked_at and now - player.name_checked_at < NAME_REFRESH_MS:
                continue
            try:
                acct = await self._riot.account_by_puuid(player.puuid, player.platform)
            except RiotError as exc:
                log.warning("name refresh: %s for %s", type(exc).__name__, player.riot_id)
                continue
            if (acct.game_name, acct.tag_line) != (player.game_name, player.tag_line):
                log.info("name refresh: %s -> %s#%s", player.riot_id,
                         acct.game_name, acct.tag_line)
                changed += 1
            await self._db.update_names(player.puuid, acct.game_name, acct.tag_line, now)
        return changed
