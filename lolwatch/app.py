"""Application wiring: build services, start-up checks, and the scheduled jobs."""

from __future__ import annotations

import logging
from datetime import datetime, time
from typing import Any

from telegram import Update
from telegram.ext import Application, ApplicationBuilder, ContextTypes

from .bot import commands
from .bot.auth import services as _svc
from .config import Config
from .db import Database, key_fingerprint
from .flusher import SessionService
from .notifier import Alerter, Notifier
from .poller import Poller
from .reports import broadcast_weekly
from .riot.cassiopeia_impl import CassiopeiaClient
from .riot.errors import RiotAuthError, RiotError
from .riot.retry import RetryingClient
from .services import Services

log = logging.getLogger(__name__)

KEY_FP = "riot_key_fingerprint"


class StartupError(RuntimeError):
    """A start-up check failed; the process should exit non-zero."""


class PtbFlushScheduler:
    """FlushScheduler backed by PTB's JobQueue: one named job per session."""

    def __init__(self, app: Application, flush_cb):
        self._app, self._cb = app, flush_cb

    def schedule(self, session_id: int, delay_s: float) -> None:
        jq = self._app.job_queue
        assert jq is not None
        name = f"flush:{session_id}"
        for job in jq.get_jobs_by_name(name):
            job.schedule_removal()
        jq.run_once(self._cb, when=max(0.0, delay_s), name=name, data=session_id)


# -- jobs --------------------------------------------------------------------

def _next_run(job: Any) -> datetime | None:
    """Job.next_t raises AttributeError until the JobQueue has started (i.e. inside
    post_init); report None there instead of crashing."""
    try:
        return job.next_t
    except AttributeError:
        return None


async def _poll_job(context: ContextTypes.DEFAULT_TYPE) -> None:
    await _svc(context).poller.poll_once()   # never raises


async def _names_job(context: ContextTypes.DEFAULT_TYPE) -> None:
    try:
        await _svc(context).poller.refresh_names()
    except Exception:
        log.exception("name refresh failed")


async def _flush_job(context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = _svc(context)
    session_id = context.job.data  # type: ignore[union-attr]
    try:
        await svc.sessions.flush(session_id)
    except Exception:
        # A bug or DB hiccup must not lose the notification: try again shortly.
        log.exception("session=%s flush job crashed; retrying", session_id)
        svc.sessions.retry_flush_soon(session_id)
        await svc.alerter.alert("flush_error", f"Flushing session {session_id} failed "
                                               "unexpectedly; will retry. See the logs.")


async def _weekly_job(context: ContextTypes.DEFAULT_TYPE) -> None:
    svc = _svc(context)
    try:
        n = await broadcast_weekly(svc.db, svc.cfg, svc.notifier)
        log.info("weekly summary: %d message(s)", n)
    except Exception:
        log.exception("weekly summary failed")
        await svc.alerter.alert("weekly_error", "The weekly summary failed. See the logs.")


# -- start-up checks ---------------------------------------------------------

async def _check_key_change(svc: Services) -> None:
    """PUUIDs are encrypted per key (spec §4.4). A changed key with stored PUUIDs
    means every one of them is silently invalid until `repuuid` runs."""
    current = key_fingerprint(svc.cfg.riot_api_key)
    stored = await svc.db.kv_get(KEY_FP)
    if stored is None or not await svc.db.list_players(active_only=False):
        await svc.db.kv_set(KEY_FP, current)
        return
    if stored != current:
        msg = ("The Riot API key has changed since the last run. Stored PUUIDs are "
               "encrypted per key and are now INVALID: run "
               "`uv run python -m lolwatch.cli repuuid`, then restart.")
        log.critical(msg)
        await svc.alerter.alert("key_changed", msg)


async def _validate_riot(svc: Services) -> None:
    """One cheap call to prove the key works. Only an auth failure is fatal."""
    players = await svc.db.list_players()
    if not players:
        log.warning("startup: no players yet, so the Riot key could not be validated")
        return
    p = players[0]
    try:
        await svc.riot.resolve_account(p.game_name, p.tag_line, p.platform)
    except RiotAuthError as exc:
        raise StartupError(f"Riot API key rejected (403): {exc}. Is it expired?") from exc
    except RiotError as exc:
        log.warning("startup: Riot validation inconclusive (%s: %s); continuing",
                    type(exc).__name__, exc)


# -- build -------------------------------------------------------------------

def build_application(cfg: Config) -> Application:
    async def post_init(app: Application) -> None:
        try:
            me = await app.bot.get_me()
        except Exception as exc:
            raise StartupError(f"Telegram token validation failed: {exc}") from exc
        log.info("telegram: connected as @%s", me.username)

        db = await Database.open(cfg.db_path)
        riot = RetryingClient(CassiopeiaClient(cfg.riot_api_key))
        notifier = Notifier(app.bot, db)
        alerter = Alerter(notifier, db, cfg.telegram.admin_user_ids)
        scheduler = PtbFlushScheduler(app, _flush_job)
        sessions = SessionService(db, riot, notifier, scheduler, cfg.notifications)
        poller = Poller(db, riot, sessions, alerter)
        svc = Services(cfg, db, riot, poller, sessions, notifier, alerter)
        app.bot_data["svc"] = svc

        await _validate_riot(svc)
        await _check_key_change(svc)

        jq = app.job_queue
        assert jq is not None
        recovered = await sessions.recover()
        log.info("recovered %d open session(s)", recovered)
        jq.run_repeating(_poll_job, interval=cfg.riot.poll_interval_seconds, first=5,
                         name="poll")
        jq.run_repeating(_names_job, interval=6 * 3600, first=120, name="names")
        if cfg.weekly.enabled:
            # tz-aware time => cron trigger in that zone, so DST shifts are handled.
            weekly = jq.run_daily(
                _weekly_job, time=time(cfg.weekly.time.hour, cfg.weekly.time.minute,
                                       tzinfo=cfg.weekly.tz),
                days=(cfg.weekly.day_index,), name="weekly")
            svc.next_weekly = lambda: _next_run(weekly)
            log.info("weekly summary scheduled: %s %s %s", cfg.weekly.day,
                     cfg.weekly.time, cfg.weekly.timezone)
        log.info("started: mode=%s poll=%ss players=%d", cfg.notifications.mode,
                 cfg.riot.poll_interval_seconds, len(await db.list_players()))

    async def post_shutdown(app: Application) -> None:
        svc = app.bot_data.get("svc")
        if svc:
            await svc.db.close()

    async def on_error(update: object, context: ContextTypes.DEFAULT_TYPE) -> None:
        log.error("unhandled telegram error", exc_info=context.error)

    app = (ApplicationBuilder().token(cfg.telegram_bot_token)
           .post_init(post_init).post_shutdown(post_shutdown).build())
    commands.register(app)
    app.add_error_handler(on_error)
    return app


def run(cfg: Config) -> None:
    app = build_application(cfg)
    app.run_polling(allowed_updates=[Update.MESSAGE], drop_pending_updates=True)
