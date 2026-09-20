"""Command handlers and access control, driven with fake Telegram objects."""

from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from conftest import MIN, T0, Env, make_match, make_player
from telegram.error import Forbidden, RetryAfter, TelegramError

from lolwatch.bot import auth, commands
from lolwatch.bot.auth import FAILURE, REFUSAL
from lolwatch.config import Config, TelegramConfig
from lolwatch.models import AccountInfo
from lolwatch.notifier import Alerter, Notifier
from lolwatch.poller import Poller
from lolwatch.riot.errors import RiotAuthError, RiotUnavailable
from lolwatch.services import Services

ADMIN, MEMBER, STRANGER = 1, 50, 99      # admin (config), allow-listed user, nobody
GROUP, PRIVATE = -1001, 5


class FakeApp:
    def __init__(self):
        self.handlers: dict[str, object] = {}
        self.filters: dict[str, object] = {}

    def add_handler(self, handler, group=0):
        for name in handler.commands:
            self.handlers[name] = handler.callback
            self.filters[name] = handler.filters


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
    await env.db.add_allowed_user(MEMBER, ADMIN, 0)
    auth._logged_strangers.clear()
    return env


async def call(env, command, *args, user=ADMIN, chat=PRIVATE, chat_type="private",
               is_bot=False, sender_chat=None, reply_to=None):
    """Run a command; returns the list of replies sent."""
    replies: list[str] = []
    message = SimpleNamespace(reply_text=AsyncMock(side_effect=lambda t, **kw: replies.append(t)),
                              sender_chat=sender_chat, reply_to_message=reply_to)
    update = SimpleNamespace(
        effective_user=SimpleNamespace(id=user, is_bot=is_bot),
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
                          ("start", []), ("stop", []), ("settings", []), ("sendweekly", []),
                          ("allow", ["77"]), ("disallow", [str(MEMBER)]), ("allowed", [])]:
        assert await call(bot_env, command, *args, user=MEMBER) == [REFUSAL], command
    assert await bot_env.db.subscribers() == []
    assert len(await bot_env.db.list_players()) == 1
    assert await bot_env.db.allowed_users() == [MEMBER]


async def test_strangers_are_ignored_completely_on_every_command(bot_env):
    for c in commands.COMMANDS:
        assert await call(bot_env, c.name, "77", user=STRANGER) == [], c.name
        assert await call(bot_env, c.name, "77", user=STRANGER, chat=GROUP,
                          chat_type="supergroup") == [], c.name
    assert await bot_env.db.subscribers() == [] and await bot_env.db.allowed_users() == [MEMBER]


async def test_group_chats_are_refused_until_explicitly_subscribed(bot_env):
    assert await call(bot_env, "add", "Faker#KR1", chat=GROUP, chat_type="group") == [REFUSAL]
    assert await call(bot_env, "list", chat=GROUP, chat_type="supergroup",
                      user=MEMBER) == [REFUSAL]

    [reply] = await call(bot_env, "start", chat=GROUP, chat_type="group")   # admin opts in
    assert "subscribed" in reply or "will now get" in reply
    assert await bot_env.db.is_subscriber(GROUP)

    [listing] = await call(bot_env, "list", chat=GROUP, chat_type="group", user=MEMBER)
    assert "Alice#EUW" in listing                       # allowed users may read there
    # ...but being in a subscribed group is not enough: the sender must be allowed.
    assert await call(bot_env, "list", chat=GROUP, chat_type="group", user=STRANGER) == []


async def test_read_commands_need_an_allowed_user_in_private_or_a_subscribed_chat(bot_env):
    assert await call(bot_env, "list", user=STRANGER, chat=STRANGER) == []
    assert "Alice#EUW" in (await call(bot_env, "list", user=ADMIN))[0]
    assert "Alice#EUW" in (await call(bot_env, "list", user=MEMBER, chat=MEMBER))[0]


async def test_refusal_reveals_nothing_about_the_command(bot_env):
    [reply] = await call(bot_env, "settings", user=MEMBER)
    assert reply == REFUSAL and "key" not in reply.lower() and "mode" not in reply.lower()


@pytest.fixture
def sentinel_commands(monkeypatch, bot_env):
    """Every command in the real table, with its handler swapped for a probe, run through
    the real registration. Covers commands added later without touching this test."""
    import dataclasses

    ran: list[str] = []

    def probe(name):
        async def handler(update, context):
            ran.append(name)
        return handler

    table = tuple(dataclasses.replace(c, handler=probe(c.name)) for c in commands.COMMANDS)
    monkeypatch.setattr(commands, "COMMANDS", table)
    app = FakeApp()
    commands.register(app)
    bot_env.app = app
    return ran, table


async def test_access_matrix_for_every_command(bot_env, sentinel_commands):
    ran, table = sentinel_commands
    await bot_env.db.add_subscriber(-2002, 0)                    # a subscribed group
    chats = {"private": (PRIVATE, "private"), "new group": (GROUP, "group"),
             "subscribed group": (-2002, "supergroup")}

    for c in table:
        for who, user in (("stranger", STRANGER), ("member", MEMBER), ("admin", ADMIN)):
            for where, (chat, kind) in chats.items():
                ran.clear()
                replies = await call(bot_env, c.name, user=user, chat=chat, chat_type=kind)
                if who == "stranger":
                    want_ran, want_reply = False, []
                elif c.access == "admin" and who == "member":
                    want_ran, want_reply = False, [REFUSAL]
                elif where == "new group" and not c.allow_new_chat:
                    want_ran, want_reply = False, [REFUSAL]
                else:
                    want_ran, want_reply = True, []
                assert (ran == [c.name], replies) == (want_ran, want_reply), \
                    f"/{c.name} as {who} in {where}"


async def test_bots_and_on_behalf_of_chat_senders_never_count_even_if_listed(
        bot_env, sentinel_commands):
    ran, table = sentinel_commands
    for c in table:
        for user in (ADMIN, MEMBER):
            assert await call(bot_env, c.name, user=user, is_bot=True) == []
            assert await call(bot_env, c.name, user=user,
                              sender_chat=SimpleNamespace(id=-100)) == []
    assert ran == []


async def test_commands_ignore_edited_messages(bot_env):
    from telegram.ext import filters

    assert set(bot_env.app.filters) == {c.name for c in commands.COMMANDS}
    assert all(f is filters.UpdateType.MESSAGE for f in bot_env.app.filters.values())


async def test_a_failing_allow_list_lookup_refuses_instead_of_crashing_open(bot_env):
    bot_env.db.is_allowed_user = AsyncMock(side_effect=RuntimeError("db down"))
    assert await call(bot_env, "list", user=MEMBER) == [FAILURE]     # never runs the command


# -- the allow list (/allow /disallow /allowed) -----------------------------

async def test_allow_and_disallow_control_who_the_bot_answers(bot_env):
    assert await call(bot_env, "list", user=77, chat=77) == []
    [reply] = await call(bot_env, "allow", "77")
    assert "77" in reply and "may now use" in reply
    assert "Alice#EUW" in (await call(bot_env, "list", user=77, chat=77))[0]
    assert "already allowed" in (await call(bot_env, "allow", "77"))[0]
    assert await bot_env.db.allowed_users() == [MEMBER, 77]

    assert "Removed" in (await call(bot_env, "disallow", "77"))[0]
    assert await call(bot_env, "list", user=77, chat=77) == []
    assert "wasn't on the list" in (await call(bot_env, "disallow", "77"))[0]


async def test_allow_accepts_a_reply_to_a_message_but_not_to_a_bot(bot_env):
    person = SimpleNamespace(from_user=SimpleNamespace(id=123, is_bot=False))
    assert "may now use" in (await call(bot_env, "allow", reply_to=person))[0]
    assert 123 in await bot_env.db.allowed_users()
    a_bot = SimpleNamespace(from_user=SimpleNamespace(id=456, is_bot=True))
    assert "Usage" in (await call(bot_env, "allow", reply_to=a_bot))[0]
    assert 456 not in await bot_env.db.allowed_users()


async def test_allow_rejects_garbage_ids_and_admins(bot_env):
    for bad in ("abc", "-5", "0", "1.5", "12345678901234567890", "²"):
        assert "Usage" in (await call(bot_env, "allow", bad))[0], bad
    assert "Usage" in (await call(bot_env, "allow", "1", "2"))[0]
    assert "Usage" in (await call(bot_env, "allow"))[0]
    assert "admin already" in (await call(bot_env, "allow", str(ADMIN)))[0]
    assert "can't be removed" in (await call(bot_env, "disallow", str(ADMIN)))[0]
    assert await bot_env.db.allowed_users() == [MEMBER]


async def test_allowed_lists_admins_and_users(bot_env):
    [text] = await call(bot_env, "allowed")
    assert f"<code>{ADMIN}</code>" in text and f"<code>{MEMBER}</code>" in text


async def test_an_allowed_user_in_a_subscribed_group_can_read_but_not_manage(bot_env):
    await bot_env.db.add_subscriber(GROUP, 0)
    assert "Alice#EUW" in (await call(bot_env, "list", user=MEMBER, chat=GROUP,
                                      chat_type="group"))[0]
    assert await call(bot_env, "allow", "77", user=MEMBER, chat=GROUP,
                      chat_type="group") == [REFUSAL]


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
    assert sent and "Week ending" in sent[0]


async def test_list_shows_the_last_game_played_not_the_polling_marker(bot_env):
    from datetime import datetime

    now = int(datetime.now().timestamp() * 1000)
    await bot_env.db.set_last_seen("puuid-alice", now)         # e.g. just added
    await bot_env.db.insert_match(make_match("m1", now - 5 * 60 * 60_000, 30))
    [text] = await call(bot_env, "list")
    assert "last game 4h ago" in text or "last game 5h ago" in text, text


# -- help / command table ---------------------------------------------------

async def test_every_registered_command_is_in_the_table_and_in_admin_help(bot_env):
    assert set(bot_env.app.handlers) == {c.name for c in commands.COMMANDS}
    assert len(commands.COMMANDS) == len({c.name for c in commands.COMMANDS})
    [text] = await call(bot_env, "help")
    for c in commands.COMMANDS:
        assert f"/{c.name}" in text, c.name


async def test_help_hides_admin_commands_from_non_admins(bot_env):
    await bot_env.db.add_subscriber(GROUP, 0)
    [text] = await call(bot_env, "help", user=MEMBER, chat=GROUP, chat_type="group")
    for c in commands.COMMANDS:
        assert (f"/{c.name}" in text) == (c.access == "subscriber"), c.name
    assert await call(bot_env, "help", user=STRANGER, chat=STRANGER) == []


def test_menu_entries_satisfy_telegrams_limits_and_respect_access():
    import re

    for entry in commands.menu(admin=True):
        assert re.fullmatch(r"[a-z0-9_]{1,32}", entry.command)
        assert 1 <= len(entry.description) <= 256
    public = {e.command for e in commands.menu(admin=False)}
    assert public == {c.name for c in commands.COMMANDS if c.access == "subscriber"}
    assert len(commands.menu(admin=True)) == len(commands.COMMANDS)


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


# -- regressions from the bug hunt --------------------------------------------

async def test_add_refuses_a_platform_where_the_account_has_no_league_profile(bot_env):
    bot_env.riot.accounts[("faker", "kr1")] = account(level=None)      # e.g. wrong platform
    [reply] = await call(bot_env, "add", "Faker#KR1", "eun1")
    assert "no League profile" in reply and "eun1" in reply
    assert await bot_env.db.get_player("puuid-faker") is None          # nothing was stored


async def test_readding_a_removed_player_does_not_announce_games_from_the_gap(bot_env):
    await call(bot_env, "remove", "Alice#EUW")
    bot_env.riot.accounts[("alice", "euw")] = account("Alice", "EUW", "puuid-alice")
    from test_poller import match_data
    bot_env.riot.ids["puuid-alice"] = ["EUW1_5"]
    bot_env.riot.matches["EUW1_5"] = match_data("EUW1_5", T0 + 5 * MIN)
    await bot_env.db.set_last_seen("puuid-alice", T0)               # marker from before /remove
    await call(bot_env, "add", "Alice#EUW")
    assert await bot_env.db.open_sessions() == []
    assert bot_env.notifier.sessions == [] and bot_env.notifier.games == []
    assert len(await bot_env.db.matches_between("puuid-alice", 0, 2**60)) == 1   # stored silently
    assert await bot_env.svc.poller.poll_once() == 0                            # and not re-announced
    assert await bot_env.db.open_sessions() == []


async def test_sendweekly_does_not_claim_success_when_nothing_was_delivered(bot_env):
    async def broadcast(text):
        return False

    bot_env.svc.notifier.broadcast = broadcast
    await bot_env.db.add_subscriber(PRIVATE, 0)
    [reply] = await call(bot_env, "sendweekly")
    assert "✅" not in reply and "delivered" in reply


async def test_stats_days_argument_must_be_plain_ascii_digits(bot_env):
    for junk in ("²", "٣", "99999"):
        [reply] = await call(bot_env, "stats", "Alice#EUW", junk)
        assert "Sorry" not in reply and "Something went wrong" not in reply, junk
