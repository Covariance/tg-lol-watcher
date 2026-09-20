"""Telegram command handlers (spec §8)."""

from __future__ import annotations

import asyncio
import logging
from datetime import datetime
from html import escape

from telegram import Update
from telegram.constants import ParseMode
from telegram.ext import Application, CommandHandler, ContextTypes

from ..config import parse_riot_id
from ..db import key_fingerprint
from ..flusher import now_ms
from ..models import Player
from ..reports import broadcast_weekly, render_window
from ..riot.errors import RiotAuthError, RiotError, RiotNotFound
from ..riot.routing import is_platform
from .auth import admin_only, services, subscriber_only

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
    if len(args) > 1 and args[-1].isdigit():
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
    await broadcast_weekly(svc.db, svc.cfg, svc.notifier)
    await _reply(update, f"✅ Weekly summary sent to {len(subscribers)} "
                         f"chat{'s' if len(subscribers) != 1 else ''}.")


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


def register(app: Application) -> None:
    app.add_handler(CommandHandler("start", admin_only(_start, allow_new_chat=True)))
    app.add_handler(CommandHandler("stop", admin_only(_stop)))
    app.add_handler(CommandHandler("add", admin_only(_add)))
    app.add_handler(CommandHandler("remove", admin_only(_remove)))
    app.add_handler(CommandHandler("settings", admin_only(_settings)))
    app.add_handler(CommandHandler("sendweekly", admin_only(_sendweekly)))
    app.add_handler(CommandHandler("list", subscriber_only(_list)))
    app.add_handler(CommandHandler("status", subscriber_only(_status)))
    app.add_handler(CommandHandler("week", subscriber_only(_week)))
    app.add_handler(CommandHandler("stats", subscriber_only(_stats)))
