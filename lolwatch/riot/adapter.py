"""The ONLY Riot surface the rest of the app sees.

Nothing outside `lolwatch/riot/` knows how Riot is reached (`http_client.py`
today). Swap the implementation behind this protocol; the app doesn't change.

Deviations from the spec's four methods, both driven by other requirements:
`account_by_puuid` (weekly display-name refresh needs PUUID -> Riot ID) and
`resolve_account` returning the summoner level (`/add` confirms it).
"""

from __future__ import annotations

from datetime import datetime
from typing import Protocol

from ..models import AccountInfo, MatchData


class RiotClient(Protocol):
    async def resolve_account(self, game_name: str, tag_line: str,
                              platform: str) -> AccountInfo:
        """Riot ID -> account. Raises RiotNotFound for an unknown ID."""
        ...

    async def account_by_puuid(self, puuid: str, platform: str) -> AccountInfo:
        """PUUID -> current Riot ID (names change; the PUUID is the identity)."""
        ...

    async def recent_match_ids(self, puuid: str, platform: str,
                               since: datetime | None,
                               count: int = 20) -> list[str]:
        """Match ids (`EUW1_123`) that ended after `since`, newest first."""
        ...

    async def get_match(self, match_id: str, platform: str) -> MatchData:
        ...

    async def is_in_game(self, puuid: str, platform: str) -> bool:
        """Live-game lookup. False when the player has no active game (404)."""
        ...
