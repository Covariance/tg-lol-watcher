"""Plain dataclasses shared across modules."""

from __future__ import annotations

from dataclasses import dataclass

# Match.win is stored as an INTEGER: 1 win, 0 loss, -1 unknown (Arena, odd modes).
WIN, LOSS, UNKNOWN = 1, 0, -1


@dataclass(frozen=True, slots=True)
class AccountInfo:
    puuid: str
    game_name: str
    tag_line: str
    summoner_level: int | None = None


@dataclass(frozen=True, slots=True)
class ParticipantResult:
    puuid: str
    champion: str
    win: int  # WIN / LOSS / UNKNOWN
    kills: int | None
    deaths: int | None
    assists: int | None


@dataclass(frozen=True, slots=True)
class MatchData:
    """A parsed match-v5 game. `participants` is keyed by PUUID."""

    match_id: str
    queue_id: int
    duration_s: int
    start_ms: int
    end_ms: int
    remake: bool
    participants: dict[str, ParticipantResult]


@dataclass(frozen=True, slots=True)
class Player:
    puuid: str
    game_name: str
    tag_line: str
    platform: str
    active: bool = True
    added_by: int | None = None
    added_at: int = 0
    last_seen_ms: int | None = None
    name_checked_at: int | None = None

    @property
    def riot_id(self) -> str:
        return f"{self.game_name}#{self.tag_line}"


@dataclass(frozen=True, slots=True)
class Match:
    """One tracked player's row in `matches`."""

    match_id: str
    puuid: str
    queue_id: int
    champion: str
    win: int
    remake: bool
    kills: int | None
    deaths: int | None
    assists: int | None
    duration_s: int
    start_ms: int
    end_ms: int
    ingested_at: int = 0


@dataclass(frozen=True, slots=True)
class Session:
    id: int
    puuid: str
    started_ms: int
    last_end_ms: int
    status: str  # 'open' | 'flushed'
    flush_due_ms: int | None
    postpones: int = 0
    notified_at: int | None = None
