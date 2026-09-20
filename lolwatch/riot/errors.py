"""Typed Riot errors. The adapter maps HTTP failures onto these at the boundary."""

from __future__ import annotations


class RiotError(Exception):
    """Base class for everything the Riot adapter raises."""


class RiotAuthError(RiotError):
    """403 — the API key is missing, invalid or (most likely) expired."""


class RiotRateLimited(RiotError):
    """429 — back off. `retry_after` is in seconds when Riot told us."""

    def __init__(self, message: str = "rate limited", retry_after: float | None = None):
        super().__init__(message)
        self.retry_after = retry_after


class RiotUnavailable(RiotError):
    """5xx / timeout / connection failure. Retry with jitter, do not alert."""


class RiotNotFound(RiotError):
    """404 — unknown Riot ID, PUUID or match."""


class RiotBadRequest(RiotError):
    """400 — most often a PUUID encrypted under a different API key (run `repuuid`)."""
