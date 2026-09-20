"""Telegram command handlers (spec §8)."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import datetime
from html import escape
from typing import Literal

from telegram import Bot, BotCommand, BotCommandScopeChat, Update
from telegram.constants import ParseMode
from telegram.error import TelegramError
from telegram.ext import Application, CommandHandler, ContextTypes, filters

from ..config import parse_riot_id
from ..db import key_fingerprint
from ..flusher import now_ms
from ..models import Player
from ..reports import broadcast_weekly, render_window
from ..riot.errors import RiotAuthError, RiotError, RiotNotFound
from ..riot.routing import is_platform
from .auth import Handler, admin_only, is_admin, services, subscriber_only

log = logging.getLogger(__name__)
HTML = ParseMode.HTML


async def _reply(update: Update, text: str) -> None:
    assert update.effective_message is not None
    await update.effective_message.reply_text(
        text, parse_mode=HTML, disable_web_page_preview=True)


def _parse_player_args(args: list[str], default_platform: str) -> tuple[str, str, str]:
    """`Game Name#TAG [platform]` -> (name, tag, platform). Names may contain spaces."""
    platform = default_platform
    if len(args) > 1 and is_platform(args[-1]):
        platform, args = args[-1].lower(), args[:-1]
    name, tag = parse_riot_id(" ".join(args))
    return name, tag, platform


# -- subscription ------------------------------------------------------------

async def _start(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    chat = update.effective_chat
    assert chat is not None
    added = await svc.db.add_subscriber(chat.id, now_ms())
    await _reply(update, "✅ This chat will now get session and weekly updates."
                 if added else "This chat is already subscribed.")


async def _stop(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    assert update.effective_chat is not None
    removed = await svc.db.remove_subscriber(update.effective_chat.id)
    await _reply(update, "Unsubscribed. Send /start to resume." if removed
                 else "This chat wasn't subscribed.")


# -- player management -------------------------------------------------------

async def _add(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    user = update.effective_user
    assert user is not None
    try:
        name, tag, platform = _parse_player_args(
            list(context.args or []), svc.cfg.riot.default_platform)
    except ValueError:
        return await _reply(update, "Usage: <code>/add GameName#TAG [platform]</code>")

    try:
        acct = await svc.riot.resolve_account(name, tag, platform)
    except RiotNotFound:
        return await _reply(update, f"No account <b>{escape(name)}#{escape(tag)}</b> found "
                                    f"(platform <code>{platform}</code>).")
    except RiotAuthError:
        return await _reply(update, "The Riot API key was rejected (likely expired). "
                                    "Update it and restart the bot.")
    except RiotError as exc:
        log.warning("/add: riot error %s: %s", type(exc).__name__, exc)
        return await _reply(update, "Riot's API isn't responding right now. Try again shortly.")

    existing = await svc.db.get_player(acct.puuid)
    if existing and existing.active:
        return await _reply(update, f"Already tracking <b>{escape(existing.riot_id)}</b>.")
    if acct.summoner_level is None:
        # Account lookups work region-wide, so a wrong platform in the same region gets
        # this far. Tracking it would silently break the live-game check and game ids.
        return await _reply(
            update, f"Found the Riot account <b>{escape(acct.game_name)}#"
                    f"{escape(acct.tag_line)}</b>, but no League profile on "
                    f"<code>{platform}</code>. Wrong platform? Try "
                    f"<code>/add {escape(acct.game_name)}#{escape(acct.tag_line)} "
                    "&lt;platform&gt;</code>. (If the platform is right, Riot may be "
                    "flaky: try again in a minute.)")

    player = Player(puuid=acct.puuid, game_name=acct.game_name, tag_line=acct.tag_line,
                    platform=platform, added_by=user.id, added_at=now_ms(),
                    name_checked_at=now_ms())
    await svc.db.add_player(player)
    level = f", level {acct.summoner_level}" if acct.summoner_level else ""
    await _reply(update, f"✅ Now tracking <b>{escape(player.riot_id)}</b> "
                         f"({platform}{level}). Importing recent games…")

    stored = await svc.db.get_player(acct.puuid)
    assert stored is not None
    try:
        # Silent: history from before we started watching is never announced.
        n = await svc.poller.ingest(stored, notify=False, count=100)
        await _reply(update, f"Imported {n} recent game{'s' if n != 1 else ''}.")
    except RiotError as exc:
        log.warning("/add: initial import failed: %s", exc)
        await _reply(update, "Couldn't import history yet; the poller will retry.")


async def _remove(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    try:
        name, tag = parse_riot_id(" ".join(context.args or []))
    except ValueError:
        return await _reply(update, "Usage: <code>/remove GameName#TAG</code>")
    player = await svc.db.find_player(name, tag)
    if player is None:
        return await _reply(update, "That player isn't being tracked.")
    await svc.db.deactivate_player(player.puuid)
    await _reply(update, f"Stopped tracking <b>{escape(player.riot_id)}</b> (history kept).")


def _ago(ms: int | None, now: int) -> str:
    if ms is None:
        return "never"
    minutes = max(0, (now - ms) // 60_000)
    if minutes < 60:
        return f"{minutes}m ago"
    if minutes < 48 * 60:
        return f"{minutes // 60}h ago"
    return f"{minutes // 1440}d ago"


async def _list(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    players = await svc.db.list_players()
    if not players:
        return await _reply(update, "No players tracked yet. Add one with /add GameName#TAG")
    now = now_ms()
    last_games = await svc.db.last_game_ends()   # not last_seen_ms: that is a polling marker
    lines = [f"• <b>{escape(p.riot_id)}</b> ({p.platform}) — last game "
             f"{_ago(last_games.get(p.puuid), now)}" for p in players]
    await _reply(update, "<b>Tracked players</b>\n" + "\n".join(lines))


async def _status(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    players = await svc.db.list_players()
    if not players:
        return await _reply(update, "No players tracked yet.")

    async def check(p: Player) -> str:
        try:
            live = await svc.riot.is_in_game(p.puuid, p.platform)
        except RiotError as exc:
            log.info("/status: %s for %s", type(exc).__name__, p.riot_id)
            return f"❔ <b>{escape(p.riot_id)}</b> — unknown (Riot API error)"
        return (f"🟢 <b>{escape(p.riot_id)}</b> — in game" if live
                else f"⚪ <b>{escape(p.riot_id)}</b> — not in game")

    lines = await asyncio.gather(*(check(p) for p in players))
    await _reply(update, "\n".join(lines))


# -- stats -------------------------------------------------------------------

async def _week(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    players = await svc.db.list_players()
    if context.args:
        try:
            name, tag = parse_riot_id(" ".join(context.args))
        except ValueError:
            return await _reply(update, "Usage: <code>/week [GameName#TAG]</code>")
        player = await svc.db.find_player(name, tag)
        if player is None:
            return await _reply(update, "That player isn't being tracked.")
        players = [player]
    for text in await render_window(svc.db, svc.cfg, players, days=7):
        await _reply(update, text)


async def _stats(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    args = list(context.args or [])
    days = 7
    if len(args) > 1 and args[-1].isascii() and args[-1].isdigit() and len(args[-1]) <= 4:
        days, args = int(args[-1]), args[:-1]
    usage = "Usage: <code>/stats GameName#TAG [days]</code>"
    if not 1 <= days <= 365:
        return await _reply(update, "Days must be between 1 and 365.")
    try:
        name, tag = parse_riot_id(" ".join(args))
    except ValueError:
        return await _reply(update, usage)
    player = await svc.db.find_player(name, tag)
    if player is None:
        return await _reply(update, "That player isn't being tracked.")
    for text in await render_window(svc.db, svc.cfg, [player], days=days):
        await _reply(update, text)


# -- admin -------------------------------------------------------------------

async def _sendweekly(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    subscribers = await svc.db.subscribers()
    if not subscribers:
        return await _reply(update, "No subscribers yet. Send /start in the chat that should "
                                    "receive updates.")
    messages, delivered = await broadcast_weekly(svc.db, svc.cfg, svc.notifier)
    if delivered < messages:
        return await _reply(update, f"⚠️ Only {delivered} of {messages} message(s) were "
                                    "delivered to anyone. Check the logs.")
    await _reply(update, f"✅ Weekly summary sent to {len(subscribers)} "
                         f"chat{'s' if len(subscribers) != 1 else ''}.")


# -- who may use the bot -----------------------------------------------------

def _target_user_id(update: Update, args: list[str]) -> int | None:
    """A numeric Telegram user id from the arguments, or from the message being replied
    to. Returns None when neither is usable (bots and non-numeric text are rejected)."""
    if args:
        text = args[0]
        if len(args) == 1 and text.isascii() and text.isdigit() and len(text) <= 15:
            return int(text) or None
        return None
    replied = getattr(update.effective_message, "reply_to_message", None)
    sender = getattr(replied, "from_user", None)
    if sender is None or getattr(sender, "is_bot", False):
        return None
    return sender.id


async def _allow(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    user = update.effective_user
    assert user is not None
    target = _target_user_id(update, list(context.args or []))
    if target is None:
        return await _reply(update, "Usage: <code>/allow &lt;user_id&gt;</code>, or reply "
                                    "to their message with <code>/allow</code>.")
    if is_admin(svc, target):
        return await _reply(update, "That user is an admin already.")
    added = await svc.db.add_allowed_user(target, user.id, now_ms())
    log.info("allow list: %s %d (by admin %d)", "added" if added else "kept", target, user.id)
    await _reply(update, f"✅ User <code>{target}</code> may now use the bot." if added
                 else f"User <code>{target}</code> was already allowed.")


async def _disallow(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    user = update.effective_user
    assert user is not None
    target = _target_user_id(update, list(context.args or []))
    if target is None:
        return await _reply(update, "Usage: <code>/disallow &lt;user_id&gt;</code>, or reply "
                                    "to their message with <code>/disallow</code>.")
    if is_admin(svc, target):
        return await _reply(update, "Admins are set in config.toml and can't be removed here.")
    removed = await svc.db.remove_allowed_user(target)
    log.info("allow list: %s %d (by admin %d)", "removed" if removed else "absent",
             target, user.id)
    await _reply(update, f"Removed <code>{target}</code>; the bot now ignores them." if removed
                 else f"User <code>{target}</code> wasn't on the list.")


async def _allowed(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    admins = svc.cfg.telegram.admin_user_ids
    users = await svc.db.allowed_users()
    lines = ["<b>Admins</b> (config.toml)", *(f"• <code>{a}</code>" for a in admins),
             "", "<b>Allowed users</b>"]
    lines += [f"• <code>{u}</code>" for u in users] or ["(none yet: /allow &lt;user_id&gt;)"]
    await _reply(update, "\n".join(lines))


async def _settings(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = services(context)
    cfg = svc.cfg
    n, w = cfg.notifications, cfg.weekly
    nxt = svc.next_weekly()
    nxt_text = (nxt.astimezone(cfg.tz).strftime("%a %d %b %H:%M %Z")  # type: ignore[attr-defined]
                if isinstance(nxt, datetime) else ("disabled" if not w.enabled else "unknown"))
    last_auth = await svc.db.kv_get("alert:riot_auth")
    key_status = ("⚠️ rejected recently (last alert "
                  f"{_ago(int(last_auth), now_ms())})" if last_auth else "ok (no auth failures logged)")
    lines = [
        "<b>Settings</b>",
        f"Notifications: <code>{n.mode}</code>, gap {n.session_gap_minutes}m, "
        f"recheck {n.flush_recheck_minutes}m, max postpones {n.max_postpones}",
        f"Polling: every {cfg.riot.poll_interval_seconds}s, default platform "
        f"<code>{cfg.riot.default_platform}</code>",
        f"Weekly: {'on' if w.enabled else 'off'} — {w.day} {w.time:%H:%M} {w.timezone}",
        f"Next weekly run: {nxt_text}",
        f"Riot key: <code>{key_fingerprint(cfg.riot_api_key)[:8]}</code> — {key_status}",
        f"Subscribers: {len(await svc.db.subscribers())}, "
        f"players: {len(await svc.db.list_players())}",
    ]
    await _reply(update, "\n".join(lines))


# -- command table -----------------------------------------------------------
# The single source of truth: handler registration, /help and Telegram's command
# menu are all generated from this, so a command cannot exist without being listed.

@dataclass(frozen=True)
class Command:
    name: str
    description: str                  # one line; also the text in Telegram's menu
    access: Literal["subscriber", "admin"]
    handler: Handler
    usage: str = ""                   # arguments, e.g. "GameName#TAG [platform]"
    allow_new_chat: bool = False      # admin commands that work in a not-yet-subscribed chat


async def _help(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    user = update.effective_user
    assert user is not None
    is_admin = user.id in services(context).cfg.telegram.admin_user_ids

    def line(c: Command) -> str:
        return f"/{c.name} {escape(c.usage)}".rstrip() + f" — {escape(c.description)}"

    lines = ["<b>Commands</b>", *(line(c) for c in COMMANDS if c.access == "subscriber")]
    if is_admin:
        lines += ["", "<b>Admin</b>", *(line(c) for c in COMMANDS if c.access == "admin")]
    await _reply(update, "\n".join(lines))


COMMANDS: tuple[Command, ...] = (
    Command("help", "Show this list", "subscriber", _help),
    Command("list", "Tracked players and when they last played", "subscriber", _list),
    Command("status", "Who is in a game right now", "subscriber", _status),
    Command("week", "Summary of the last 7 days", "subscriber", _week,
            usage="[GameName#TAG]"),
    Command("stats", "Summary for any window (default 7 days)", "subscriber", _stats,
            usage="GameName#TAG [days]"),
    Command("start", "Subscribe this chat to updates", "admin", _start, allow_new_chat=True),
    Command("stop", "Unsubscribe this chat", "admin", _stop),
    Command("add", "Track a player (imports recent games silently)", "admin", _add,
            usage="GameName#TAG [platform]"),
    Command("remove", "Stop tracking a player (history is kept)", "admin", _remove,
            usage="GameName#TAG"),
    Command("sendweekly", "Send the weekly summary to all subscribers now", "admin",
            _sendweekly),
    Command("allow", "Let a Telegram user use the bot (or reply to their message)", "admin",
            _allow, usage="[user_id]"),
    Command("disallow", "Stop the bot answering a user", "admin", _disallow,
            usage="[user_id]"),
    Command("allowed", "Who may use the bot", "admin", _allowed),
    Command("settings", "Effective config, key status, next weekly run", "admin", _settings),
)


def register(app: Application) -> None:
    for c in COMMANDS:
        handler = (admin_only(c.handler, allow_new_chat=c.allow_new_chat)
                   if c.access == "admin" else subscriber_only(c.handler))
        # Only fresh messages: an edited message must not re-run a command.
        app.add_handler(CommandHandler(c.name, handler, filters=filters.UpdateType.MESSAGE))


def menu(*, admin: bool) -> list[BotCommand]:
    """Commands for Telegram's autocomplete menu (the list shown when typing `/`)."""
    return [BotCommand(c.name, c.description) for c in COMMANDS
            if admin or c.access == "subscriber"]


async def publish_menu(bot: Bot, admin_ids: Iterable[int]) -> None:
    """Everyone sees the read commands; each admin's private chat also gets the admin
    ones. Failures are logged, never fatal: the menu is a convenience."""
    try:
        await bot.set_my_commands(menu(admin=False))
        for admin_id in admin_ids:
            await bot.set_my_commands(menu(admin=True), scope=BotCommandScopeChat(admin_id))
    except TelegramError as exc:
        log.warning("could not publish the command menu: %s", exc)
