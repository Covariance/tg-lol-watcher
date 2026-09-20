"""Session coalescing — PURE LOGIC.

No `datetime.now()`, no I/O: every function takes explicit times, so the subtle
cases (out-of-order arrival, postpones, restarts) are unit-testable with a
frozen clock. `flusher.py` is the I/O shell around these decisions.
"""

from __future__ import annotations

from collections.abc import Iterable, Sequence
from dataclasses import dataclass

from .models import Match, Session


@dataclass(frozen=True, slots=True)
class Assignment:
    """What to do with a newly ingested match."""

    attach: bool             # True: join the open session. False: start a new one.
    flush_open: bool         # True: the open session is stale and must flush right now.
    started_ms: int          # resulting session bounds
    last_end_ms: int
    flush_due_ms: int


def assign(open_session: Session | None, start_ms: int, end_ms: int,
           gap_ms: int) -> Assignment:
    """Decide where a match belongs.

    A match joins the open session when its interval is within `gap_ms` of the
    session's [started, last_end] span. Out-of-order arrival is handled with
    min/max, and `flush_due_ms` is always re-derived from the true latest end.
    """
    if open_session is not None:
        starts_before_gap_closes = start_ms <= open_session.last_end_ms + gap_ms
        ends_after_gap_opens = end_ms >= open_session.started_ms - gap_ms
        if starts_before_gap_closes and ends_after_gap_opens:
            last_end = max(open_session.last_end_ms, end_ms)
            return Assignment(attach=True, flush_open=False,
                              started_ms=min(open_session.started_ms, start_ms),
                              last_end_ms=last_end, flush_due_ms=last_end + gap_ms)
        # Entirely older than the open session (late arrival): it's its own
        # historical session and the open one is untouched. Otherwise the open
        # session is stale (e.g. we were down) and is flushed before the new one.
        older = end_ms + gap_ms < open_session.started_ms
        return Assignment(attach=False, flush_open=not older, started_ms=start_ms,
                          last_end_ms=end_ms, flush_due_ms=end_ms + gap_ms)
    return Assignment(attach=False, flush_open=False, started_ms=start_ms,
                      last_end_ms=end_ms, flush_due_ms=end_ms + gap_ms)


@dataclass(frozen=True, slots=True)
class FlushDecision:
    flush: bool
    postpones: int
    flush_due_ms: int | None   # new due time when postponing
    reason: str                # for the DEBUG decision log


def decide_flush(*, postpones: int, in_game: bool | None, max_postpones: int,
                 now_ms: int, recheck_ms: int) -> FlushDecision:
    """The flush-job decision.

    `in_game=None` means the spectator lookup failed. That is deliberately
    treated as "not in game": a spectator outage must delay nothing.
    """
    if not in_game:
        reason = "spectator unavailable, treating as not in game" if in_game is None \
            else "not in game"
        return FlushDecision(True, postpones, None, reason)
    postpones += 1
    if postpones > max_postpones:
        return FlushDecision(True, postpones, None,
                             f"still in game but postpones {postpones} > max {max_postpones}")
    return FlushDecision(False, postpones, now_ms + recheck_ms,
                         f"in game, postponing ({postpones}/{max_postpones})")


def flush_delay_s(session: Session, now_ms: int) -> float:
    """Seconds until the session's flush check should run. 0 if overdue —
    this is what restart recovery uses so a restart can't swallow a notification."""
    due = session.flush_due_ms if session.flush_due_ms is not None else now_ms
    return max(0.0, (due - now_ms) / 1000)


def group_matches(matches: Iterable[Match], gap_ms: int) -> list[list[Match]]:
    """Split matches into sessions by the same gap rule (used by summaries)."""
    ordered: Sequence[Match] = sorted(matches, key=lambda m: m.start_ms)
    groups: list[list[Match]] = []
    latest_end = 0
    for m in ordered:
        if groups and m.start_ms <= latest_end + gap_ms:
            groups[-1].append(m)
            latest_end = max(latest_end, m.end_ms)
        else:
            groups.append([m])
            latest_end = m.end_ms
    return groups
