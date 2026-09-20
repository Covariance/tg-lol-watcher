"""Start-up wiring: the real `post_init`, with Riot and Telegram faked out.

Catches the class of bug unit tests can't: wrong PTB JobQueue usage, start-up
ordering, and the "refuse to start" checks (spec §10).
"""

from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from conftest import MIN, FakeRiot, make_match
from telegram import Bot

from lolwatch import app as app_module
from lolwatch.app import KEY_FP, StartupError, _flush_job, _poll_job, build_application
from lolwatch.config import load_config
from lolwatch.db import Database, key_fingerprint
from lolwatch.flusher import now_ms
from lolwatch.models import AccountInfo
from lolwatch.riot.errors import RiotAuthError, RiotUnavailable

CONFIG = """
[telegram]
admin_user_ids = [777]
[weekly]
day = "sunday"
time = "20:00"
timezone = "Europe/London"
"""


@pytest.fixture
def riot(monkeypatch):
    fake = FakeRiot()
    fake.accounts[("faker", "kr1")] = AccountInfo("puuid-faker", "Faker", "KR1", 700)
    monkeypatch.setattr(app_module, "CassiopeiaClient", lambda key: fake)
    monkeypatch.setattr(app_module, "RetryingClient", lambda inner: inner)   # no real backoff sleeps
    return fake


@pytest.fixture
def send(monkeypatch):
    """Fake Telegram: get_me + send_message, recording what would have been sent."""
    sent: list[tuple[int, str]] = []

    async def send_message(self, chat_id, text, **kw):
        sent.append((chat_id, text))

    monkeypatch.setattr(Bot, "get_me", AsyncMock(return_value=SimpleNamespace(username="tb")))
    monkeypatch.setattr(Bot, "send_message", send_message)
    return sent


@pytest.fixture
def make_app(tmp_path):
    def build():
        path = tmp_path / "config.toml"
        path.write_text(CONFIG, encoding="utf8")
        cfg = load_config(path, {"RIOT_API_KEY": "RGAPI-one", "TELEGRAM_BOT_TOKEN": "1:x",
                                 "LOLWATCH_DB": str(tmp_path / "bot.db")})
        return cfg, build_application(cfg)
    return build


async def start(app):
    await app.post_init(app)
    return app.bot_data["svc"]


async def test_startup_registers_jobs_and_records_the_key(make_app, riot, send):
    cfg, app = make_app()
    svc = await start(app)
    try:
        assert await svc.db.list_players() == []      # players come only from the DB
        assert await svc.db.kv_get(KEY_FP) == key_fingerprint("RGAPI-one")
        jq = app.job_queue
        for name in ("poll", "names", "weekly"):
            assert jq.get_jobs_by_name(name), f"job {name!r} not scheduled"
        assert svc.next_weekly() is None          # before the queue starts: None, not a crash
    finally:
        await svc.db.close()


async def test_weekly_job_fires_on_the_configured_day_in_the_local_timezone(make_app, riot, send):
    cfg, app = make_app()
    svc = await start(app)
    try:
        await app.job_queue.start()
        nxt = svc.next_weekly()
        assert nxt is not None
        local = nxt.astimezone(cfg.tz)
        assert (local.strftime("%A"), local.hour, local.minute) == ("Sunday", 20, 0)
        assert local.utcoffset() == nxt.utcoffset()           # tz-aware trigger, not a fixed UTC hour
    finally:
        await app.job_queue.stop()
        await svc.db.close()


async def test_restart_reschedules_open_sessions(make_app, riot, send, tmp_path):
    cfg, _ = make_app()
    db = await Database.open(cfg.db_path)
    await db.add_subscriber(-5, 0)
    from conftest import make_player
    await db.add_player(make_player("puuid-faker", "Faker", "KR1"))
    await db.insert_match(make_match("m1", now_ms() - 60 * MIN, 30, puuid="puuid-faker"))
    sid = await db.create_session("puuid-faker", now_ms() - 60 * MIN, now_ms() - 30 * MIN,
                                  now_ms() - 15 * MIN)          # overdue
    await db.attach_match(sid, "m1", "puuid-faker")
    await db.kv_set(KEY_FP, key_fingerprint("RGAPI-one"))
    await db.close()

    _, app = make_app()
    svc = await start(app)
    try:
        assert app.job_queue.get_jobs_by_name(f"flush:{sid}")
        await app.job_queue.start()
        await _flush_job(SimpleNamespace(application=app, job=SimpleNamespace(data=sid)))
        assert [chat for chat, _ in send if chat == -5], "session summary was not delivered"
        assert (await svc.db.get_session(sid)).status == "flushed"
    finally:
        await app.job_queue.stop()
        await svc.db.close()


async def test_changed_api_key_triggers_a_loud_repuuid_warning(make_app, riot, send, caplog):
    cfg, _ = make_app()
    db = await Database.open(cfg.db_path)
    from conftest import make_player
    await db.add_player(make_player("puuid-faker", "Faker", "KR1"))
    await db.kv_set(KEY_FP, key_fingerprint("RGAPI-OLD-DEV-KEY"))
    await db.close()

    _, app = make_app()
    with caplog.at_level("CRITICAL"):
        svc = await start(app)
    try:
        assert any("repuuid" in r.message for r in caplog.records)
        assert any(chat == 777 and "repuuid" in text for chat, text in send)   # admin alerted
        assert await svc.db.kv_get(KEY_FP) == key_fingerprint("RGAPI-OLD-DEV-KEY")  # not silenced
    finally:
        await svc.db.close()


async def test_startup_refuses_when_validation_call_is_rejected(make_app, riot, send):
    cfg, _ = make_app()
    db = await Database.open(cfg.db_path)
    from conftest import make_player
    await db.add_player(make_player("puuid-faker", "Faker", "KR1"))
    await db.kv_set(KEY_FP, key_fingerprint("RGAPI-one"))
    await db.close()
    riot.error = RiotAuthError("403 Forbidden")

    _, app = make_app()
    with pytest.raises(StartupError, match="expired"):
        await app.post_init(app)
    await app.bot_data["svc"].db.close()


async def test_riot_outage_at_startup_is_not_fatal(make_app, riot, send):
    _, app = make_app()
    riot.error = RiotUnavailable("503")
    svc = await start(app)                     # validates nothing, still boots
    try:
        assert await svc.db.list_players() == []
    finally:
        await svc.db.close()


async def test_startup_refuses_on_a_bad_telegram_token(make_app, riot, monkeypatch):
    monkeypatch.setattr(Bot, "get_me", AsyncMock(side_effect=RuntimeError("Unauthorized")))
    _, app = make_app()
    with pytest.raises(StartupError, match="Telegram"):
        await app.post_init(app)


async def test_poll_job_never_raises_even_if_riot_explodes(make_app, riot, send):
    _, app = make_app()
    svc = await start(app)
    from conftest import make_player
    await svc.db.add_player(make_player("puuid-faker", "Faker", "KR1"))
    try:
        riot.error = ValueError("kaboom")
        await _poll_job(SimpleNamespace(application=app))       # must not raise
        assert any(chat == 777 for chat, _ in send)              # operator told, once
    finally:
        await svc.db.close()
