"""Session coalescing (spec §5): pure functions first, then the service on a frozen clock."""

from __future__ import annotations

from conftest import MIN, T0, Env, FakeScheduler, make_match, make_player

from lolwatch.config import NotificationConfig
from lolwatch.models import Session
from lolwatch.riot.errors import RiotUnavailable
from lolwatch.sessions import assign, decide_flush, flush_delay_s, group_matches

GAP = 15 * MIN


def session(start=T0, end=T0 + 30 * MIN, postpones=0, due=None) -> Session:
    return Session(id=1, puuid="p", started_ms=start, last_end_ms=end, status="open",
                   flush_due_ms=end + GAP if due is None else due, postpones=postpones)


# -- pure logic --------------------------------------------------------------

def test_assign_first_match_creates_session_due_after_gap():
    a = assign(None, T0, T0 + 30 * MIN, GAP)
    assert (a.attach, a.flush_open) == (False, False)
    assert (a.started_ms, a.last_end_ms, a.flush_due_ms) == (T0, T0 + 30 * MIN, T0 + 45 * MIN)


def test_assign_within_gap_attaches_and_extends():
    a = assign(session(), T0 + 35 * MIN, T0 + 65 * MIN, GAP)
    assert a.attach and not a.flush_open
    assert a.last_end_ms == T0 + 65 * MIN
    assert a.flush_due_ms == T0 + 80 * MIN


def test_assign_gap_boundary_is_inclusive():
    a = assign(session(), T0 + 30 * MIN + GAP, T0 + 90 * MIN, GAP)
    assert a.attach
    b = assign(session(), T0 + 30 * MIN + GAP + 1, T0 + 90 * MIN, GAP)
    assert not b.attach


def test_assign_after_gap_flushes_stale_session_and_starts_new_one():
    a = assign(session(), T0 + 70 * MIN, T0 + 100 * MIN, GAP)
    assert (a.attach, a.flush_open) == (False, True)
    assert a.started_ms == T0 + 70 * MIN


def test_assign_out_of_order_keeps_true_latest_end():
    # A game that ended *before* the session's current last_end arrives late.
    a = assign(session(start=T0 + 40 * MIN, end=T0 + 70 * MIN),
               T0 + 20 * MIN, T0 + 38 * MIN, GAP)
    assert a.attach
    assert a.started_ms == T0 + 20 * MIN
    assert a.last_end_ms == T0 + 70 * MIN            # max(), not overwritten by the older game
    assert a.flush_due_ms == T0 + 70 * MIN + GAP     # re-derived from the true latest end


def test_assign_much_older_match_does_not_flush_or_join_open_session():
    a = assign(session(start=T0 + 300 * MIN, end=T0 + 330 * MIN), T0, T0 + 30 * MIN, GAP)
    assert (a.attach, a.flush_open) == (False, False)


def test_decide_flush_not_in_game_flushes():
    d = decide_flush(postpones=0, in_game=False, max_postpones=18, now_ms=T0, recheck_ms=5 * MIN)
    assert d.flush


def test_decide_flush_spectator_error_is_not_a_reason_to_postpone():
    d = decide_flush(postpones=0, in_game=None, max_postpones=18, now_ms=T0, recheck_ms=5 * MIN)
    assert d.flush and "unavailable" in d.reason


def test_decide_flush_in_game_postpones_then_gives_up():
    d = decide_flush(postpones=0, in_game=True, max_postpones=2, now_ms=T0, recheck_ms=5 * MIN)
    assert (d.flush, d.postpones, d.flush_due_ms) == (False, 1, T0 + 5 * MIN)
    d = decide_flush(postpones=2, in_game=True, max_postpones=2, now_ms=T0, recheck_ms=5 * MIN)
    assert d.flush and d.postpones == 3


def test_flush_delay_overdue_is_zero():
    assert flush_delay_s(session(due=T0 - 5 * MIN), T0) == 0
    assert flush_delay_s(session(due=T0 + 90_000), T0) == 90


def test_group_matches_splits_on_gap():
    ms = [make_match("a", T0), make_match("b", T0 + 35 * MIN), make_match("c", T0 + 200 * MIN)]
    assert [[m.match_id for m in g] for g in group_matches(ms, GAP)] == [["a", "b"], ["c"]]


# -- service, frozen clock ---------------------------------------------------

async def test_two_games_5_minutes_apart_one_session_one_notification(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    await env.ingest(p, make_match("m2", T0 + 35 * MIN, 30))     # 5 min after m1 ended

    assert len(await env.db.open_sessions()) == 1
    sid, delay = env.scheduler.last
    assert delay == (65 + 15) * 60 - (env.clock.now - T0) / 1000  # due = last_end + gap

    env.clock.set(T0 + 80 * MIN)
    await env.service.flush(sid)

    assert len(env.notifier.sessions) == 1
    assert [m.match_id for m in env.notifier.sessions[0][1]] == ["m1", "m2"]


async def test_two_games_40_minutes_apart_two_sessions_two_notifications(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    env.clock.set(T0 + 30 * MIN)
    await env.ingest(p, make_match("m2", T0 + 70 * MIN, 30))     # 40 min after m1 ended

    # m1's session was closed the moment the later game showed it was over.
    assert len(env.notifier.sessions) == 1
    assert [m.match_id for m in env.notifier.sessions[0][1]] == ["m1"]

    sid, _ = env.scheduler.last
    env.clock.set(T0 + 115 * MIN)
    await env.service.flush(sid)
    assert [[m.match_id for m in ms] for _, ms in env.notifier.sessions] == [["m1"], ["m2"]]


async def test_in_game_postpones_and_next_game_joins_the_same_session(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    sid, _ = env.scheduler.last

    env.clock.set(T0 + 45 * MIN)          # flush due, but the player queued up again
    env.riot.in_game = True
    await env.service.flush(sid)
    assert env.notifier.sessions == []
    assert env.scheduler.last == (sid, 5 * 60)
    stored = await env.db.get_session(sid)
    assert stored.postpones == 1 and stored.status == "open"

    # The game they were in finishes; it started 10 min after m1 ended.
    env.clock.set(T0 + 75 * MIN)
    await env.ingest(p, make_match("m2", T0 + 40 * MIN, 30))
    assert len(await env.db.open_sessions()) == 1              # same session, not a new one
    assert (await env.db.get_session(sid)).postpones == 0      # activity resets the counter

    env.riot.in_game = False
    env.clock.set(T0 + 85 * MIN)
    await env.service.flush(sid)
    assert len(env.notifier.sessions) == 1
    assert [m.match_id for m in env.notifier.sessions[0][1]] == ["m1", "m2"]


async def test_max_postpones_exceeded_flushes_anyway(db):
    e = Env(db, NotificationConfig(max_postpones=2))
    p = await e.add_player()
    await e.ingest(p, make_match("m1", T0, 30))
    sid, _ = e.scheduler.last
    e.riot.in_game = True                  # spectator "lies" forever

    e.clock.set(T0 + 45 * MIN)
    await e.service.flush(sid)             # postpone 1
    e.clock.set(T0 + 50 * MIN)
    await e.service.flush(sid)             # postpone 2
    assert e.notifier.sessions == []
    e.clock.set(T0 + 55 * MIN)
    await e.service.flush(sid)             # 3 > max_postpones -> flush
    assert len(e.notifier.sessions) == 1
    assert (await e.db.get_session(sid)).status == "flushed"


async def test_out_of_order_arrival_yields_correct_last_end(env: Env):
    p = make_player()
    await env.ingest(p, make_match("late", T0 + 40 * MIN, 30))    # ends T0+70
    await env.ingest(p, make_match("early", T0, 30))              # arrives second, ended earlier
    sid, _ = env.scheduler.last
    s = await env.db.get_session(sid)
    assert s.started_ms == T0
    assert s.last_end_ms == T0 + 70 * MIN
    assert s.flush_due_ms == T0 + 85 * MIN
    assert len(await env.db.open_sessions()) == 1


async def test_restart_with_open_session_still_flushes_once(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    sid, _ = env.scheduler.last

    # "Process dies": all in-memory jobs are gone. A fresh service + scheduler start up
    # on the same database while the session is already overdue.
    env.clock.set(T0 + 3 * 60 * MIN)
    fresh_scheduler = FakeScheduler()
    restarted = env.make_service(fresh_scheduler)
    assert await restarted.recover() == 1
    assert fresh_scheduler.calls == [(sid, 0.0)]      # overdue -> immediate check

    await restarted.flush(sid)
    await restarted.flush(sid)                        # a duplicate job must be harmless
    assert len(env.notifier.sessions) == 1


async def test_recover_reschedules_future_due_time(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    env.clock.set(T0 + 35 * MIN)                      # 10 min until due (T0+45)
    scheduler = FakeScheduler()
    await env.make_service(scheduler).recover()
    assert scheduler.calls == [(1, 600.0)]


async def test_single_game_is_exactly_one_notification(env: Env):
    p = make_player()
    await env.ingest(p, make_match("solo", T0, 25))
    sid, _ = env.scheduler.last
    env.clock.set(T0 + 40 * MIN)
    await env.service.flush(sid)
    assert len(env.notifier.sessions) == 1
    assert len(env.notifier.sessions[0][1]) == 1


async def test_spectator_outage_does_not_delay_the_flush(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    sid, _ = env.scheduler.last
    env.riot.in_game = RiotUnavailable("spectator down")
    env.clock.set(T0 + 45 * MIN)
    await env.service.flush(sid)
    assert len(env.notifier.sessions) == 1


async def test_flush_job_firing_early_reschedules_instead_of_dropping(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    sid, _ = env.scheduler.last
    env.clock.set(T0 + 44 * MIN)                      # one minute before due
    await env.service.flush(sid)
    assert env.notifier.sessions == []
    assert env.scheduler.last == (sid, 60.0)


async def test_delivery_failure_retries_instead_of_losing_the_notification(env: Env):
    p = make_player()
    await env.ingest(p, make_match("m1", T0, 30))
    sid, _ = env.scheduler.last
    env.clock.set(T0 + 45 * MIN)
    env.notifier.ok = False
    await env.service.flush(sid)
    assert (await env.db.get_session(sid)).status == "open"
    assert env.scheduler.last == (sid, 300.0)

    env.notifier.ok = True
    env.clock.set(T0 + 50 * MIN)
    await env.service.flush(sid)
    assert len(env.notifier.sessions) == 1


async def test_per_game_mode_sends_each_game_and_creates_no_sessions(db):
    e = Env(db, NotificationConfig(mode="per_game"))
    p = await e.add_player()
    await e.ingest(p, make_match("m1", T0, 30))
    await e.ingest(p, make_match("m2", T0 + 35 * MIN, 30))
    assert [m.match_id for _, m in e.notifier.games] == ["m1", "m2"]
    assert await e.db.open_sessions() == []


async def test_off_mode_sends_nothing(db):
    e = Env(db, NotificationConfig(mode="off"))
    p = await e.add_player()
    await e.ingest(p, make_match("m1", T0, 30))
    assert e.notifier.games == [] and e.notifier.sessions == []
    assert e.scheduler.calls == []
