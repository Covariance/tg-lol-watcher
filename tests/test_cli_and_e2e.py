"""Operator CLI paths and the spec's headline behaviours (§12), end to end."""

from __future__ import annotations

import argparse

from conftest import MIN, T0, Env, make_match, make_player
from test_poller import make_poller, match_data

from lolwatch import cli
from lolwatch.app import KEY_FP
from lolwatch.config import load_config
from lolwatch.db import Database, key_fingerprint
from lolwatch.models import AccountInfo
from lolwatch.riot.errors import RiotNotFound

CONFIG = """
[telegram]
admin_user_ids = [1]
[weekly]
timezone = "UTC"
"""


def cfg_for(tmp_path, key="RGAPI-new"):
    path = tmp_path / "config.toml"
    path.write_text(CONFIG, encoding="utf8")
    return load_config(path, {"RIOT_API_KEY": key, "LOLWATCH_DB": str(tmp_path / "cli.db")},
                       need_telegram=False)


async def seed(cfg):
    db = await Database.open(cfg.db_path)
    await db.add_player(make_player("OLD-PUUID", "Alice", "EUW"))
    await db.insert_match(make_match("m1", T0, puuid="OLD-PUUID"))
    sid = await db.create_session("OLD-PUUID", T0, T0 + 30 * MIN, T0 + 45 * MIN)
    await db.attach_match(sid, "m1", "OLD-PUUID")
    await db.kv_set(KEY_FP, key_fingerprint("RGAPI-old"))
    await db.close()


async def test_repuuid_rewrites_every_table_and_records_the_new_key(tmp_path, monkeypatch, env):
    cfg = cfg_for(tmp_path)
    await seed(cfg)
    env.riot.accounts[("alice", "euw")] = AccountInfo("NEW-PUUID", "Alice", "EUW", 30)
    monkeypatch.setattr(cli, "_riot", lambda c: env.riot)

    assert await cli.cmd_repuuid(cfg, argparse.Namespace()) == 0

    db = await Database.open(cfg.db_path)
    try:
        assert await db.get_player("OLD-PUUID") is None
        assert (await db.get_player("NEW-PUUID")).riot_id == "Alice#EUW"
        assert len(await db.matches_between("NEW-PUUID", 0, 2**60)) == 1
        assert (await db.open_sessions("NEW-PUUID"))[0].puuid == "NEW-PUUID"
        assert await db.kv_get(KEY_FP) == key_fingerprint("RGAPI-new")   # warning now silenced
    finally:
        await db.close()


async def test_repuuid_leaves_the_warning_armed_if_someone_cannot_be_resolved(
        tmp_path, monkeypatch, env, capsys):
    cfg = cfg_for(tmp_path)
    await seed(cfg)

    async def missing(*a):
        raise RiotNotFound("renamed")
    env.riot.resolve_account = missing
    monkeypatch.setattr(cli, "_riot", lambda c: env.riot)

    assert await cli.cmd_repuuid(cfg, argparse.Namespace()) == 1
    assert "NOT FOUND" in capsys.readouterr().err
    db = await Database.open(cfg.db_path)
    try:
        assert await db.kv_get(KEY_FP) == key_fingerprint("RGAPI-old")   # not marked as fixed
        assert await db.get_player("OLD-PUUID") is not None
    finally:
        await db.close()


async def test_dry_run_summary_prints_plain_text_and_sends_nothing(tmp_path, capsys):
    from datetime import datetime

    cfg = cfg_for(tmp_path)
    db = await Database.open(cfg.db_path)
    await db.add_player(make_player())
    now_ms = int(datetime.now().timestamp() * 1000)
    await db.insert_match(make_match("m1", now_ms - 60 * MIN, 30))
    await db.close()

    assert await cli.cmd_dry_run_summary(cfg, argparse.Namespace(days=7)) == 0
    out = capsys.readouterr().out
    assert "Week ending" in out and "Alice#EUW" in out and "1 game" in out
    assert "<b>" not in out and "<pre>" not in out


# -- spec §12: definition of done -------------------------------------------

async def test_five_back_to_back_games_send_one_message_after_the_last_not_during_the_fourth(
        env: Env):
    poller, _ = make_poller(env)
    env.clock.set(T0 - 10 * MIN)
    await poller.poll_once()                              # first sight: history baseline
    ids: list[str] = []

    def game(i):                                          # 25 min games, 3 min queue between
        start = T0 + i * 28 * MIN
        mid = f"EUW1_{i}"
        ids.append(mid)
        env.riot.matches[mid] = match_data(mid, start, 25)
        env.riot.ids["puuid-alice"] = list(reversed(ids))
        return start + 25 * MIN

    sent_before_last = 0
    for i in range(5):
        end = game(i)
        env.clock.set(end + 2 * MIN)                      # poll two minutes after each ends
        await poller.poll_once()
        if i < 4:
            sent_before_last += len(env.notifier.sessions)
        sid, delay = env.scheduler.last
        # the flush check is always scheduled for "last end + 15 min", so it can't fire mid-session
        assert env.clock.now + delay * 1000 == end + 15 * MIN

    assert sent_before_last == 0                          # silence through game four
    assert len(await env.db.open_sessions()) == 1

    env.clock.set(end + 15 * MIN)
    await env.service.flush(sid)
    assert len(env.notifier.sessions) == 1
    assert len(env.notifier.sessions[0][1]) == 5


async def test_restart_mid_session_still_yields_exactly_one_notification(env: Env):
    poller, _ = make_poller(env)
    env.clock.set(T0 - MIN)
    await poller.poll_once()
    env.riot.ids["puuid-alice"] = ["EUW1_1"]
    env.riot.matches["EUW1_1"] = match_data("EUW1_1", T0, 30)
    env.clock.set(T0 + 32 * MIN)
    await poller.poll_once()
    sid, _ = env.scheduler.last

    # crash + restart while the session is open; a *new* poller also re-polls the same game
    env.clock.set(T0 + 50 * MIN)
    from conftest import FakeScheduler
    scheduler = FakeScheduler()
    service = env.make_service(scheduler)
    await service.recover()
    from lolwatch.poller import Poller
    await Poller(env.db, env.riot, service, None, clock=env.clock).poll_once()
    await service.flush(sid)
    await service.flush(sid)
    assert len(env.notifier.sessions) == 1
