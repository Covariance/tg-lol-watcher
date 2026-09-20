"""Telegram delivery: broadcast to subscribers, and deduped operator alerts."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Callable

from telegram import Bot
from telegram.constants import ParseMode
from telegram.error import Forbidden, RetryAfter, TelegramError

from .db import Database
from .flusher import now_ms
from .formatting import TELEGRAM_LIMIT, format_game, format_session
from .models import Match, Player

log = logging.getLogger(__name__)

ALERT_COOLDOWN_MS = 60 * 60 * 1000   # once per hour per alert key


class Notifier:
    def __init__(self, bot: Bot, db: Database):
        self._bot, self._db = bot, db

    async def send_to(self, chat_id: int, text: str) -> bool:
        """Send one HTML message. Returns False (and logs) instead of raising."""
        if len(text) > TELEGRAM_LIMIT:
            log.error("message of %d chars exceeds Telegram limit; truncating", len(text))
            text = text[:TELEGRAM_LIMIT - 1] + "…"
        for attempt in (1, 2):
            try:
                await self._bot.send_message(
                    chat_id, text, parse_mode=ParseMode.HTML,
                    disable_web_page_preview=True)
                return True
            except RetryAfter as exc:
                if attempt == 2:
                    log.warning("telegram flood control persisted for chat %s", chat_id)
                    return False
                delay = exc.retry_after
                await asyncio.sleep(delay.total_seconds() if hasattr(delay, "total_seconds")
                                    else float(delay))
            except Forbidden as exc:
                log.warning("telegram refused chat %s (%s); is the bot blocked/removed?",
                            chat_id, exc)
                return False
            except TelegramError as exc:
                log.warning("telegram send to %s failed: %s", chat_id, exc)
                return False
        return False

    async def broadcast(self, text: str) -> bool:
        """Send to every subscriber. True if delivered somewhere, or if there is
        nobody to deliver to (nothing to retry)."""
        chats = await self._db.subscribers()
        if not chats:
            log.warning("no subscribers; message dropped (send /start from an admin chat)")
            return True
        results = [await self.send_to(chat, text) for chat in chats]
        return any(results)

    async def send_session(self, player: Player, matches: list[Match]) -> bool:
        return await self.broadcast(format_session(player, matches))

    async def send_game(self, player: Player, match: Match) -> bool:
        return await self.broadcast(format_game(player, match))


class Alerter:
    """Operator alerts (expired key, poll failures) to the admin allowlist,
    deduped through `kv` so a persistent fault pings at most once an hour."""

    def __init__(self, notifier: Notifier, db: Database, admin_ids: tuple[int, ...],
                 clock: Callable[[], int] = now_ms):
        self._notifier, self._db, self._admins, self._clock = notifier, db, admin_ids, clock

    async def alert(self, key: str, text: str) -> bool:
        """Returns True if an alert was actually sent (i.e. not deduped)."""
        kv_key = f"alert:{key}"
        now = self._clock()
        last = await self._db.kv_get(kv_key)
        if last is not None and now - int(last) < ALERT_COOLDOWN_MS:
            log.debug("alert %s suppressed (sent %ds ago)", key, (now - int(last)) // 1000)
            return False
        # Record first: if Telegram is down we still don't hammer it every poll.
        await self._db.kv_set(kv_key, str(now))
        log.warning("ALERT %s: %s", key, text)
        for admin in self._admins:
            await self._notifier.send_to(admin, f"⚠️ {text}")
        return True
