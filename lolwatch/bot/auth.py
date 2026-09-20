"""Access control for command handlers.

- Mutating commands: admin allowlist only.
- Read commands: any subscribed chat (admins are always trusted in private chat).
- Group chats: refused unless the chat is an explicit subscriber.
Refusals are one polite line — never a stack trace, never a hint about what the
command would have done.
"""

from __future__ import annotations

import functools
import logging
from collections.abc import Awaitable, Callable

from telegram import Update
from telegram.ext import ContextTypes

from ..services import Services

log = logging.getLogger(__name__)

Handler = Callable[[Update, ContextTypes.DEFAULT_TYPE], Awaitable[None]]
REFUSAL = "Sorry, I can't do that here."
FAILURE = "Something went wrong on my side. It has been logged."


def services(context: ContextTypes.DEFAULT_TYPE) -> Services:
    return context.application.bot_data["svc"]


async def _refuse(update: Update) -> None:
    if update.effective_message:
        await update.effective_message.reply_text(REFUSAL)


def _guarded(fn: Handler) -> Handler:
    @functools.wraps(fn)
    async def wrapper(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        try:
            await fn(update, context)
        except Exception:
            log.exception("command handler %s failed", fn.__name__)
            if update.effective_message:
                await update.effective_message.reply_text(FAILURE)
    return wrapper


def admin_only(fn: Handler, *, allow_new_chat: bool = False) -> Handler:
    """`allow_new_chat=True` is for /start, which is how a group becomes a subscriber."""
    @_guarded
    @functools.wraps(fn)
    async def wrapper(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        svc = services(context)
        user, chat = update.effective_user, update.effective_chat
        if user is None or chat is None or user.id not in svc.cfg.telegram.admin_user_ids:
            return await _refuse(update)
        if (not allow_new_chat and chat.type != "private"
                and not await svc.db.is_subscriber(chat.id)):
            return await _refuse(update)
        await fn(update, context)
    return wrapper


def subscriber_only(fn: Handler) -> Handler:
    @_guarded
    @functools.wraps(fn)
    async def wrapper(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        svc = services(context)
        user, chat = update.effective_user, update.effective_chat
        if user is None or chat is None:
            return await _refuse(update)
        is_admin = user.id in svc.cfg.telegram.admin_user_ids
        allowed = await svc.db.is_subscriber(chat.id) or (is_admin and chat.type == "private")
        if not allowed:
            return await _refuse(update)
        await fn(update, context)
    return wrapper
