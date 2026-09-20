"""Access control for everything the bot does.

Three layers, each failing closed:

1. Gatekeeper (`install`): a handler in group -1 that runs before every other handler.
   Only admins (config.toml) and whitelisted users (the `allowed_users` table, managed
   with /allow and /disallow) get past it; everyone else is ignored *silently* and the
   update goes no further. A handler added in the future is covered without touching it.
2. Decorators (`admin_only`, `subscriber_only`): each command re-checks the same
   whitelist, then applies its own rule. Belt and braces: the gate is one registration,
   a decorator is on every handler.
3. Chat rule: outside a private chat, a command only works in a chat an admin has
   subscribed with /start.

Roles:
- Mutating commands: admins only (never DB-stored users).
- Read commands: admins and whitelisted users, in private chat or a subscribed chat.
- Messages from bots, or sent on behalf of a chat or channel (anonymous group admins,
  linked channels), never count as a user, whatever ids are on the list.

Whitelisted users who hit a command above their role get one polite line, never a stack
trace and never a hint about what the command would have done. Strangers get nothing.
"""

from __future__ import annotations

import functools
import logging
from collections.abc import Awaitable, Callable
from typing import Any

from telegram import Update
from telegram.ext import Application, ApplicationHandlerStop, ContextTypes, TypeHandler

from ..services import Services

log = logging.getLogger(__name__)

Handler = Callable[[Update, ContextTypes.DEFAULT_TYPE], Awaitable[None]]
REFUSAL = "Sorry, I can't do that here."
FAILURE = "Something went wrong on my side. It has been logged."

_MAX_LOGGED_STRANGERS = 1000
_logged_strangers: set[int] = set()


def services(context: ContextTypes.DEFAULT_TYPE) -> Services:
    return context.application.bot_data["svc"]


def is_admin(svc: Services, user_id: int) -> bool:
    return user_id in svc.cfg.telegram.admin_user_ids


def _human_sender(update: Update) -> Any | None:
    """The sending user, or None for anything that isn't a real person speaking as
    themselves (no user, a bot, or a message sent on behalf of a chat)."""
    user = update.effective_user
    if user is None or getattr(user, "is_bot", False):
        return None
    message = update.effective_message
    if message is not None and getattr(message, "sender_chat", None) is not None:
        return None
    return user


async def may_use_bot(svc: Services, update: Update) -> bool:
    """The whitelist check: an admin, or a user an admin has allowed."""
    user = _human_sender(update)
    if user is None:
        return False
    return is_admin(svc, user.id) or await svc.db.is_allowed_user(user.id)


def _log_stranger(update: Update) -> None:
    user, chat = update.effective_user, update.effective_chat
    uid = getattr(user, "id", None)
    if uid is None or uid in _logged_strangers or len(_logged_strangers) >= _MAX_LOGGED_STRANGERS:
        return
    _logged_strangers.add(uid)
    log.warning("ignored a message from user_id=%s in chat_id=%s (not on the allow list; "
                "an admin can run /allow %s)", uid, getattr(chat, "id", None), uid)


# -- layer 1: the gate ---------------------------------------------------------

async def _gate(update: object, context: ContextTypes.DEFAULT_TYPE) -> None:
    if not isinstance(update, Update):
        raise ApplicationHandlerStop
    try:
        svc = context.application.bot_data.get("svc")
        allowed = svc is not None and await may_use_bot(svc, update)
    except Exception:
        # A failing check must block, not wave everything through: an exception in a
        # handler does not stop the other handler groups.
        log.exception("access check failed; refusing the update")
        raise ApplicationHandlerStop from None
    if not allowed:
        _log_stranger(update)
        raise ApplicationHandlerStop


def install(app: Application) -> None:
    """Register the gatekeeper ahead of every other handler (group -1 runs first)."""
    app.add_handler(TypeHandler(Update, _gate), group=-1)


# -- layer 2 and 3: per-command rules --------------------------------------------

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
        user, chat = _human_sender(update), update.effective_chat
        if user is None or chat is None or not await may_use_bot(svc, update):
            return _log_stranger(update)
        if not is_admin(svc, user.id):
            return await _refuse(update)
        if (not allow_new_chat and chat.type != "private"
                and not await svc.db.is_subscriber(chat.id)):
            return await _refuse(update)
        await fn(update, context)
    return wrapper


def subscriber_only(fn: Handler) -> Handler:
    """Read commands: an allowed user, in a private chat or in a subscribed chat."""
    @_guarded
    @functools.wraps(fn)
    async def wrapper(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
        svc = services(context)
        chat = update.effective_chat
        if chat is None or not await may_use_bot(svc, update):
            return _log_stranger(update)
        if chat.type != "private" and not await svc.db.is_subscriber(chat.id):
            return await _refuse(update)
        await fn(update, context)
    return wrapper
