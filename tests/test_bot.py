"""Command handlers and access control, driven with fake Telegram objects."""

from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from conftest import MIN, T0, Env, make_match, make_player
from telegram.error import Forbidden, RetryAfter, TelegramError

from lolwatch.bot import commands
from lolwatch.bot.auth import FAILURE, REFUSAL
from lolwatch.config import Config, TelegramConfig
from lolwatch.models import AccountInfo
from lolwatch.notifier import Alerter, Notifier
from lolwatch.poller import Poller
from lolwatch.riot.errors import RiotAuthError, RiotUnavailable
from lolwatch.services import Services

ADMIN, STRANGER = 1, 99
GROUP, PRIVATE = -1001, 5


class FakeApp:
    def __init__(self):
        self.handlers: dict[str, object] = {}

    def add_handler(self, handler):
        for name in handler.commands:
            self.handlers[name] = handler.callback


@pytest.fixture
async def bot_env(env: Env):
    cfg = Config(telegram=TelegramConfig(admin_user_ids=(ADMIN,)), riot_api_key="k")
    poller = Poller(env.db, env.riot, env.service, None, clock=env.clock)
    notifier = SimpleNamespace()
    svc = Services(cfg, env.db, env.riot, poller, env.service, notifier,
                   Alerter(notifier, env.db, (ADMIN,)))
    app = FakeApp()
    commands.register(app)
    env.svc, env.app = svc, app
    return env


async def call(env, command, *args, user=ADMIN, chat=PRIVATE, chat_type="private"):
    """Run a command; returns the list of replies sent."""
    replies: list[str] = []
    message = SimpleNamespace(reply_text=AsyncMock(side_effect=lambda t, **kw: replies.append(t)))
    update = SimpleNamespace(
        effective_user=SimpleNamespace(id=user),
        effective_chat=SimpleNamespace(id=chat, type=chat_type),
        effective_message=message)
    context = SimpleNamespace(args=list(args),
                              application=SimpleNamespace(bot_data={"svc": env.svc}))
    await env.app.handlers[command](update, context)
    return replies


def account(name="Faker", tag="KR1", puuid="puuid-faker", level=712):
    return AccountInfo(puuid, name, tag, level)


# -- access control ----------------------------------------------------------

async def test_non_admin_cannot_mutate_and_gets_one_polite_line(bot_env):
    for command, args in [("add", ["Faker#KR1"]), ("remove", ["Alice#EUW"]),
                          ("start", []), ("stop", []), ("settings", []), ("sendweekly", [])]:
        assert await call(bot_env, command, *args, user=STRANGER) == [REFUSAL]
    assert await bot_env.db.subscribers() == []
    assert len(await bot_env.db.list_players()) == 1


async def test_group_chats_are_refused_until_explicitly_subscribed(bot_env):
    assert await call(bot_env, "add", "Faker#KR1", chat=GROUP, chat_type="group") == [REFUSAL]
    assert await call(bot_env, "list", chat=GROUP, chat_type="supergroup",
                      user=STRANGER) == [REFUSAL]

    [reply] = await call(bot_env, "start", chat=GROUP, chat_type="group")   # admin opts in
    assert "subscribed" in reply or "will now get" in reply
    assert await bot_env.db.is_subscriber(GROUP)

    [listing] = await call(bot_env, "list", chat=GROUP, chat_type="group", user=STRANGER)
    assert "Alice#EUW" in listing                       # any member of a subscribed chat may read


async def test_read_commands_need_a_subscribed_chat_or_an_admin_in_private(bot_env):
    assert await call(bot_env, "list", user=STRANGER, chat=STRANGER) == [REFUSAL]
    assert "Alice#EUW" in (await call(bot_env, "list", user=ADMIN))[0]


async def test_refusal_reveals_nothing_about_the_command(bot_env):
    [reply] = await call(bot_env, "settings", user=STRANGER)
    assert reply == REFUSAL and "key" not in reply.lower() and "mode" not in reply.lower()


async def test_start_and_stop_manage_subscription(bot_env):
    await call(bot_env, "start")
    assert await bot_env.db.is_subscriber(PRIVATE)
    assert "already" in (await call(bot_env, "start"))[0]
    await call(bot_env, "stop")
    assert not await bot_env.db.is_subscriber(PRIVATE)


async def test_handler_crash_becomes_a_generic_message_not_a_traceback(bot_env):
    bot_env.svc.db.list_players = AsyncMock(side_effect=RuntimeError("secret internals"))
    replies = await call(bot_env, "list")
    assert replies == [FAILURE] and "secret" not in replies[0]


# -- /add /remove ------------------------------------------------------------

async def test_add_resolves_stores_and_imports_history_silently(bot_env):
    bot_env.riot.accounts[("faker", "kr1")] = account()
    bot_env.riot.ids["puuid-faker"] = ["KR_1"]
    from test_poller import match_data
    bot_env.riot.matches["KR_1"] = match_data("KR_1", T0, puuids=("puuid-faker",))

    replies = await call(bot_env, "add", "Faker#KR1", "kr")
    assert "Now tracking" in replies[0] and "level 712" in replies[0] and "kr" in replies[0]
    assert "Imported 1 recent game." in replies[1]

    player = await bot_env.db.get_player("puuid-faker")
    assert player.platform == "kr" and player.added_by == ADMIN
    assert await bot_env.db.open_sessions() == []          # old games are never announced


async def test_add_accepts_names_with_spaces_and_defaults_the_platform(bot_env):
    bot_env.riot.accounts[("some name", "euw")] = account("Some Name", "EUW", "puuid-sn")
    replies = await call(bot_env, "add", "Some", "Name#EUW")
    assert "Some Name#EUW" in replies[0]
    assert (await bot_env.db.get_player("puuid-sn")).platform == "euw1"


async def test_add_bad_usage_unknown_account_and_duplicates(bot_env):
    assert "Usage" in (await call(bot_env, "add", "no-tag-here"))[0]

    async def not_found(*a):
        from lolwatch.riot.errors import RiotNotFound
        raise RiotNotFound("nope")
    bot_env.riot.resolve_account = not_found
    assert "No account" in (await call(bot_env, "add", "Ghost#000"))[0]
    assert await bot_env.db.find_player("Ghost", "000") is None

    async def existing(*a):
        return AccountInfo("puuid-alice", "Alice", "EUW", 30)
    bot_env.riot.resolve_account = existing
    assert "Already tracking" in (await call(bot_env, "add", "Alice#EUW"))[0]


async def test_add_when_riot_is_down_or_key_is_dead_says_so_plainly(bot_env):
    bot_env.riot.error = RiotUnavailable("503")
    assert "isn't responding" in (await call(bot_env, "add", "A#1"))[0]
    bot_env.riot.error = RiotAuthError("403")
    assert "key was rejected" in (await call(bot_env, "add", "A#1"))[0]


async def test_remove_soft_deletes(bot_env):
    assert "Stopped tracking" in (await call(bot_env, "remove", "alice#euw"))[0]
    assert await bot_env.db.list_players() == []
    assert (await bot_env.db.find_player("Alice", "EUW", active_only=False)) is not None
    assert "isn't being tracked" in (await call(bot_env, "remove", "Alice#EUW"))[0]


# -- read commands -----------------------------------------------------------

async def test_status_reports_live_games_and_survives_riot_errors(bot_env):
    await bot_env.add_player(make_player("puuid-bob", "Bob", "EUW"))
    bot_env.riot.in_game = True
    [text] = await call(bot_env, "status")
    assert text.count("🟢") == 2 and "in game" in text

    bot_env.riot.in_game = RiotUnavailable("down")
    [text] = await call(bot_env, "status")
    assert text.count("❔") == 2


async def test_week_and_stats_render_from_the_database(bot_env):
    from datetime import datetime

    now_ms = int(datetime.now().timestamp() * 1000)
    await bot_env.db.insert_match(make_match("m1", now_ms - 90 * MIN, 30))
    await bot_env.db.insert_match(make_match("m2", now_ms - 45 * MIN, 30, win=0))

    replies = await call(bot_env, "week")
    assert "Week of" in replies[0] and "Alice#EUW" in replies[0] and "2 games" in replies[0]

    [stats] = await call(bot_env, "stats", "Alice#EUW", "3")
    assert "2 games" in stats and "50% WR" in stats

    assert "between 1 and 365" in (await call(bot_env, "stats", "Alice#EUW", "9999"))[0]
    assert "Usage" in (await call(bot_env, "stats", "garbage"))[0]
    assert "isn't being tracked" in (await call(bot_env, "stats", "Nobody#1"))[0]
    assert "isn't being tracked" in (await call(bot_env, "week", "Nobody#1"))[0]


async def test_settings_shows_effective_config_but_never_the_key(bot_env):
    [text] = await call(bot_env, "settings")
    assert "session" in text and "Next weekly run" in text and "Subscribers: 0" in text
    assert "RGAPI" not in text and "Riot key" in text


async def test_sendweekly_broadcasts_the_summary_to_subscribers(bot_env):
    sent: list[str] = []

    async def broadcast(text):
        sent.append(text)
        return True

    bot_env.svc.notifier.broadcast = broadcast
    assert "No subscribers" in (await call(bot_env, "sendweekly"))[0]
    assert sent == []

    await bot_env.db.add_subscriber(PRIVATE, 0)
    [reply] = await call(bot_env, "sendweekly")
    assert "sent to 1 chat" in reply
    assert sent and "Week of" in sent[0]


async def test_list_shows_the_last_game_played_not_the_polling_marker(bot_env):
    from datetime import datetime

    now = int(datetime.now().timestamp() * 1000)
    await bot_env.db.set_last_seen("puuid-alice", now)         # e.g. just added
    await bot_env.db.insert_match(make_match("m1", now - 5 * 60 * 60_000, 30))
    [text] = await call(bot_env, "list")
    assert "last game 4h ago" in text or "last game 5h ago" in text, text


# -- notifier ----------------------------------------------------------------

class FakeBot:
    def __init__(self, *outcomes):
        self.outcomes = list(outcomes)
        self.sent: list[tuple[int, str]] = []

    async def send_message(self, chat_id, text, **kwargs):
        outcome = self.outcomes.pop(0) if self.outcomes else None
        if isinstance(outcome, Exception):
            raise outcome
        self.sent.append((chat_id, text))


async def test_notifier_broadcasts_to_every_subscriber(env: Env):
    await env.db.add_subscriber(1, T0)
    await env.db.add_subscriber(2, T0)
    bot = FakeBot()
    assert await Notifier(bot, env.db).send_session(make_player(), [make_match("a", T0)])
    assert [c for c, _ in bot.sent] == [1, 2]


async def test_notifier_with_no_subscribers_drops_quietly(env: Env):
    assert await Notifier(FakeBot(), env.db).broadcast("hi") is True    # nothing to retry


async def test_notifier_reports_failure_so_the_session_can_retry(env: Env):
    await env.db.add_subscriber(1, T0)
    n = Notifier(FakeBot(TelegramError("network")), env.db)
    assert await n.broadcast("hi") is False
    n = Notifier(FakeBot(Forbidden("bot was blocked")), env.db)
    assert await n.broadcast("hi") is False


async def test_notifier_honours_flood_control_once(env: Env, monkeypatch):
    await env.db.add_subscriber(1, T0)
    slept = []
    monkeypatch.setattr("lolwatch.notifier.asyncio.sleep", AsyncMock(side_effect=slept.append))
    bot = FakeBot(RetryAfter(3))
    assert await Notifier(bot, env.db).broadcast("hi") is True
    assert slept == [3.0] and len(bot.sent) == 1


async def test_notifier_truncates_an_oversized_message_instead_of_failing(env: Env):
    await env.db.add_subscriber(1, T0)
    bot = FakeBot()
    await Notifier(bot, env.db).broadcast("x" * 5000)
    assert len(bot.sent[0][1]) <= 4096
