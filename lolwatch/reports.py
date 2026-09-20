"""Glue: DB matches -> aggregated summary -> Telegram messages."""

from __future__ import annotations

from collections.abc import Sequence
from datetime import datetime

from .config import Config
from .db import Database
from .formatting import format_weekly
from .models import Player
from .notifier import Notifier
from .summary import build_summary, window_days


async def render_window(db: Database, cfg: Config, players: Sequence[Player],
                        days: int = 7, now: datetime | None = None, *,
                        rolling: bool = False) -> list[str]:
    """Summary of the last `days` calendar days (local tz) as message strings.
    `rolling` uses exactly `days`x24h instead (see `window_days`)."""
    tz = cfg.tz
    now = (now or datetime.now(tz)).astimezone(tz)
    start, end = window_days(now, days, rolling=rolling)
    start_ms, end_ms = int(start.timestamp() * 1000), int(end.timestamp() * 1000) + 1
    per_player = [(p.riot_id, await db.matches_between(p.puuid, start_ms, end_ms))
                  for p in players]
    summary = build_summary(start, end, tz, per_player, cfg.notifications.gap_ms)
    return format_weekly(summary, tz, days=days, rolling=rolling)


async def broadcast_weekly(db: Database, cfg: Config, notifier: Notifier) -> tuple[int, int]:
    """Send the last-7-days summary to every subscriber.
    Returns (messages, messages delivered to at least one chat)."""
    texts = await render_window(db, cfg, await db.list_players(), days=7, rolling=True)
    delivered = 0
    for text in texts:
        delivered += bool(await notifier.broadcast(text))
    return len(texts), delivered
