"""Weekly / windowed aggregation. Pure: matches in, dataclasses out."""

from __future__ import annotations

from collections import Counter
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from datetime import date, datetime, time, timedelta, tzinfo

from .models import LOSS, WIN, Match
from .queues import group_sort_key, queue_group
from .sessions import group_matches


def _percent(part: int, whole: int) -> int:
    """Whole-number percentage, rounding half up (Python's round() is banker's:
    round(62.5) == 62, which would print 10W 6L as 62% instead of 63%)."""
    return (200 * part + whole) // (2 * whole)


@dataclass(frozen=True, slots=True)
class QueueStats:
    group: str
    games: int
    seconds: int
    wins: int
    losses: int

    @property
    def winrate(self) -> int | None:
        decided = self.wins + self.losses
        return _percent(self.wins, decided) if decided else None


@dataclass(frozen=True, slots=True)
class LongestSession:
    games: int
    started_ms: int


@dataclass(frozen=True, slots=True)
class PlayerSummary:
    riot_id: str
    games: int = 0
    seconds: int = 0
    wins: int = 0
    losses: int = 0
    remakes: int = 0
    groups: tuple[QueueStats, ...] = ()
    top_champions: tuple[tuple[str, int], ...] = ()
    longest_session: LongestSession | None = None

    @property
    def winrate(self) -> int | None:
        decided = self.wins + self.losses
        return _percent(self.wins, decided) if decided else None


@dataclass(frozen=True, slots=True)
class Summary:
    start: date
    end: date
    players: tuple[PlayerSummary, ...] = field(default_factory=tuple)


def window_days(now: datetime, days: int) -> tuple[datetime, datetime]:
    """[start, end) for the last `days` calendar days ending now, in now's tz.

    Starts at local midnight so `days=7` on a Sunday evening is exactly
    Monday 00:00 .. now — the "week" the weekly summary reports.
    """
    if now.tzinfo is None:
        raise ValueError("now must be timezone-aware")
    first_day = now.date() - timedelta(days=days - 1)
    return datetime.combine(first_day, time.min, tzinfo=now.tzinfo), now


def summarize_player(riot_id: str, matches: Iterable[Match], gap_ms: int) -> PlayerSummary:
    matches = list(matches)
    if not matches:
        return PlayerSummary(riot_id=riot_id)

    wins = sum(1 for m in matches if not m.remake and m.win == WIN)
    losses = sum(1 for m in matches if not m.remake and m.win == LOSS)
    remakes = sum(1 for m in matches if m.remake)

    by_group: dict[str, list[Match]] = {}
    for m in matches:
        by_group.setdefault(queue_group(m.queue_id), []).append(m)
    groups = tuple(
        QueueStats(
            group=name, games=len(ms), seconds=sum(m.duration_s for m in ms),
            wins=sum(1 for m in ms if not m.remake and m.win == WIN),
            losses=sum(1 for m in ms if not m.remake and m.win == LOSS))
        for name, ms in sorted(by_group.items(), key=lambda kv: group_sort_key(kv[0])))

    champs = Counter(m.champion for m in matches)
    # Highest count first; ties broken alphabetically so output is deterministic.
    top = tuple(sorted(champs.items(), key=lambda kv: (-kv[1], kv[0]))[:3])

    longest = max(group_matches(matches, gap_ms), key=len)
    longest_session = (LongestSession(len(longest), longest[0].start_ms)
                       if len(longest) >= 2 else None)

    return PlayerSummary(
        riot_id=riot_id, games=len(matches), seconds=sum(m.duration_s for m in matches),
        wins=wins, losses=losses, remakes=remakes, groups=groups, top_champions=top,
        longest_session=longest_session)


def build_summary(start: datetime, end: datetime, tz: tzinfo,
                  players: Sequence[tuple[str, Iterable[Match]]], gap_ms: int) -> Summary:
    return Summary(
        start=start.astimezone(tz).date(), end=end.astimezone(tz).date(),
        players=tuple(summarize_player(rid, ms, gap_ms) for rid, ms in players))
