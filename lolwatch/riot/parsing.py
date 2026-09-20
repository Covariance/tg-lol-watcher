"""Pure match-v5 `info` -> MatchData parsing. No I/O, no Cassiopeia.

This is where the known traps in the spec live (duration units, remakes, Arena),
so it is kept separate from any transport and tested against recorded JSON.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from ..models import LOSS, UNKNOWN, WIN, MatchData, ParticipantResult
from ..queues import NO_WIN_LOSS_QUEUES

MAX_GAME_S = 4 * 3600          # longer than this is a parsing bug, not a long game
REMAKE_UNDER_S = 5 * 60

# Data Dragon ids that differ from the in-game display name.
_DISPLAY_NAMES = {"MonkeyKing": "Wukong", "FiddleSticks": "Fiddlesticks"}


class MatchParseError(ValueError):
    """The match payload is unusable (missing timestamps, absurd duration...)."""


def _sane(seconds: float) -> bool:
    return 0 < seconds <= MAX_GAME_S


def derive_times(info: Mapping[str, Any]) -> tuple[int, int, int]:
    """Return (start_ms, end_ms, duration_s).

    Prefers gameEndTimestamp - gameStartTimestamp. `gameDuration` is seconds on
    patch 11.20+ but milliseconds on older matches, so it is only a fallback and
    is unit-corrected.
    """
    start = info.get("gameStartTimestamp") or info.get("gameCreation")
    if not start:
        raise MatchParseError("match has no gameStartTimestamp/gameCreation")
    start = int(start)

    end = info.get("gameEndTimestamp")
    if end and info.get("gameStartTimestamp"):
        seconds = (int(end) - start) / 1000
        if _sane(seconds):
            return start, int(end), round(seconds)

    raw = info.get("gameDuration")
    if raw:
        seconds = float(raw)
        if not _sane(seconds):
            seconds /= 1000  # pre-11.20 millisecond durations
        if _sane(seconds):
            return start, start + round(seconds * 1000), round(seconds)

    raise MatchParseError(
        f"cannot derive a sane duration (start={start}, end={end}, gameDuration={raw})")


def _result(queue_id: int, participant: Mapping[str, Any]) -> int:
    # Arena and other placement modes: `win` doesn't mean what it means on the Rift.
    if queue_id in NO_WIN_LOSS_QUEUES:
        return UNKNOWN
    win = participant.get("win")
    if isinstance(win, bool):
        return WIN if win else LOSS
    return UNKNOWN


def _int_or_none(value: Any) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _champion(p: Mapping[str, Any]) -> str:
    name = p.get("championName")
    if name:
        return _DISPLAY_NAMES.get(name, name)
    cid = p.get("championId")
    return f"Champion {cid}" if cid is not None else "Unknown"


def parse_match(match_id: str, info: Mapping[str, Any]) -> MatchData:
    """Parse the `info` object of a match-v5 response."""
    start_ms, end_ms, duration_s = derive_times(info)
    queue_id = _int_or_none(info.get("queueId")) or 0

    participants: dict[str, ParticipantResult] = {}
    early_surrender = False
    for p in info.get("participants") or ():
        early_surrender = early_surrender or bool(p.get("gameEndedInEarlySurrender"))
        puuid = p.get("puuid")
        if not puuid or puuid == "BOT":
            continue
        participants[puuid] = ParticipantResult(
            puuid=puuid,
            champion=_champion(p),
            win=_result(queue_id, p),
            kills=_int_or_none(p.get("kills")),
            deaths=_int_or_none(p.get("deaths")),
            assists=_int_or_none(p.get("assists")),
        )

    return MatchData(
        match_id=match_id,
        queue_id=queue_id,
        duration_s=duration_s,
        start_ms=start_ms,
        end_ms=end_ms,
        remake=early_surrender or duration_s < REMAKE_UNDER_S,
        participants=participants,
    )
