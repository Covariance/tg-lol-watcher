"""Ingestion: idempotency, silent first-time import, error isolation."""

from __future__ import annotations

from datetime import UTC, datetime

from conftest import MIN, T0, Env, make_player

from lolwatch.models import MatchData, ParticipantResult
from lolwatch.notifier import ALERT_COOLDOWN_MS, Alerter
from lolwatch.poller import OVERLAP_MS, Poller
from lolwatch.riot.errors import RiotAuthError, RiotNotFound, RiotUnavailable


def match_data(match_id: str, start_ms: int, minutes: int = 30,
               puuids: tuple[str, ...] = ("puuid-alice",), win: int = 1) -> MatchData:
    return MatchData(
        match_id=match_id, queue_id=420, duration_s=minutes * 60, start_ms=start_ms,
        end_ms=start_ms + minutes * MIN, remake=False,
        participants={p: ParticipantResult(p, "Azir", win, 5, 2, 7) for p in puuids})


class RecordingNotifier:
    """Stands in for Notifier inside Alerter: records operator alerts."""

    def __init__(self):
        self.sent: list[tuple[int, str]] = []

    async def send_to(self, chat_id, text):
        self.sent.append((chat_id, text))
        return True


def make_poller(env: Env, with_alerter: bool = True):
    alerts = RecordingNotifier()
    alerter = Alerter(alerts, env.db, (42,), clock=env.clock) if with_alerter else None
    return Poller(env.db, env.riot, env.service, alerter, clock=env.clock), alerts


async def test_first_time_player_history_is_stored_silently(env: Env):
    poller, _ = make_poller(env)
    env.clock.set(T0 + 600 * MIN)
    env.riot.ids["puuid-alice"] = ["EUW1_1", "EUW1_2"]
    env.riot.matches["EUW1_1"] = match_data("EUW1_1", T0)
    env.riot.matches["EUW1_2"] = match_data("EUW1_2", T0 + 40 * MIN)

    assert await poller.poll_once() == 2
    assert len(await env.db.matches_between("puuid-alice", 0, 2**60)) == 2
    assert await env.db.open_sessions() == []          # nothing announced
    assert env.scheduler.calls == []
    stored = await env.db.get_player("puuid-alice")
    assert stored.last_seen_ms >= env.clock.now        # so the *next* game is live


async def test_new_game_after_first_poll_is_announced(env: Env):
    poller, _ = make_poller(env)
    env.clock.set(T0)
    await poller.poll_once()                            # first sight, no games
    assert (await env.db.get_player("puuid-alice")).last_seen_ms == T0

    env.clock.set(T0 + 40 * MIN)
    env.riot.ids["puuid-alice"] = ["EUW1_9"]
    env.riot.matches["EUW1_9"] = match_data("EUW1_9", T0 + 5 * MIN)
    assert await poller.poll_once() == 1
    assert len(await env.db.open_sessions()) == 1
    assert env.scheduler.calls                          # flush job scheduled


async def test_ingestion_is_idempotent(env: Env):
    poller, _ = make_poller(env)
    await env.db.set_last_seen("puuid-alice", T0)
    env.riot.ids["puuid-alice"] = ["EUW1_9"]
    env.riot.matches["EUW1_9"] = match_data("EUW1_9", T0 + 5 * MIN)

    assert await poller.poll_once() == 1
    assert await poller.poll_once() == 0
    assert await poller.poll_once() == 0
    assert env.riot.get_match_calls == ["EUW1_9"]       # fetched once, never again
    assert len(await env.db.matches_between("puuid-alice", 0, 2**60)) == 1
    assert len(await env.db.open_sessions()) == 1       # and announced once


async def test_since_is_last_seen_minus_overlap_and_first_time_is_a_week_back(env: Env):
    poller, _ = make_poller(env)
    env.clock.set(T0)
    await poller.poll_once()
    _, since, count = env.riot.id_calls[-1]
    assert since == datetime.fromtimestamp((T0 - 7 * 86_400_000) / 1000, tz=UTC)
    assert count == 20

    await poller.poll_once()
    _, since, _ = env.riot.id_calls[-1]
    assert since == datetime.fromtimestamp((T0 - OVERLAP_MS) / 1000, tz=UTC)


async def test_a_match_shared_by_two_tracked_players_is_fetched_once(env: Env):
    bob = await env.add_player(make_player("puuid-bob", "Bob", "EUW"))
    poller, _ = make_poller(env)
    await env.db.set_last_seen("puuid-alice", T0)
    await env.db.set_last_seen(bob.puuid, T0)
    both = ("puuid-alice", "puuid-bob")
    env.riot.ids = {p: ["EUW1_5"] for p in both}
    env.riot.matches["EUW1_5"] = match_data("EUW1_5", T0 + MIN, puuids=both)

    assert await poller.poll_once() == 2
    assert env.riot.get_match_calls == ["EUW1_5"]


async def test_missing_or_unusable_matches_are_skipped_not_fatal(env: Env):
    poller, _ = make_poller(env)
    await env.db.set_last_seen("puuid-alice", T0)
    env.riot.ids["puuid-alice"] = ["EUW1_404", "EUW1_other", "EUW1_ok"]

    async def get_match(match_id, platform):
        env.riot.get_match_calls.append(match_id)
        if match_id == "EUW1_404":
            raise RiotNotFound("gone")
        return {"EUW1_other": match_data("EUW1_other", T0, puuids=("someone-else",)),
                "EUW1_ok": match_data("EUW1_ok", T0 + MIN)}[match_id]

    env.riot.get_match = get_match
    assert await poller.poll_once() == 1
    await poller.poll_once()
    assert env.riot.get_match_calls.count("EUW1_404") == 1      # not retried every poll


async def test_auth_error_alerts_once_per_hour_and_never_raises(env: Env):
    poller, alerts = make_poller(env)
    env.riot.error = RiotAuthError("403")
    for _ in range(5):
        assert await poller.poll_once() == 0
    assert len(alerts.sent) == 1
    assert alerts.sent[0][0] == 42 and "expired" in alerts.sent[0][1]

    env.clock.set(T0 + ALERT_COOLDOWN_MS + 1)
    await poller.poll_once()
    assert len(alerts.sent) == 2


async def test_unavailable_riot_is_quiet(env: Env):
    poller, alerts = make_poller(env)
    env.riot.error = RiotUnavailable("503")
    assert await poller.poll_once() == 0
    assert alerts.sent == []


async def test_unexpected_exception_does_not_kill_the_loop(env: Env):
    poller, alerts = make_poller(env)
    env.riot.error = ValueError("boom")
    assert await poller.poll_once() == 0                # returned normally
    assert len(alerts.sent) == 1 and "boom" in alerts.sent[0][1]

    env.riot.error = None                                # next tick works again
    await env.db.set_last_seen("puuid-alice", T0)
    env.riot.ids["puuid-alice"] = ["EUW1_1"]
    env.riot.matches["EUW1_1"] = match_data("EUW1_1", T0 + MIN)
    assert await poller.poll_once() == 1


async def test_backfill_is_silent_even_for_a_known_player(env: Env):
    poller, _ = make_poller(env)
    await env.db.set_last_seen("puuid-alice", T0)
    env.clock.set(T0 + 100 * MIN)
    env.riot.ids["puuid-alice"] = ["EUW1_1"]
    env.riot.matches["EUW1_1"] = match_data("EUW1_1", T0 + MIN)
    player = await env.db.get_player("puuid-alice")
    assert await poller.backfill(player, days=14) == 1
    assert await env.db.open_sessions() == []
    assert env.riot.id_calls[-1][2] == 100              # asks for a full page


async def test_refresh_names_updates_stale_display_names(env: Env):
    from lolwatch.models import AccountInfo
    poller, _ = make_poller(env)
    env.riot.by_puuid["puuid-alice"] = AccountInfo("puuid-alice", "NewName", "NEW")
    assert await poller.refresh_names() == 1
    p = await env.db.get_player("puuid-alice")
    assert (p.game_name, p.tag_line, p.name_checked_at) == ("NewName", "NEW", env.clock.now)
    assert await poller.refresh_names() == 0            # fresh: not re-checked

