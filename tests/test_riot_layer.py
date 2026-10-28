"""Riot adapter internals verified offline: retry policy, the HTTP client against a
mock transport (URLs, routing, error mapping), and the rate limiter. No network."""

from __future__ import annotations

from datetime import UTC, datetime

import httpx
import pytest
from conftest import FakeRiot, load_fixture

from lolwatch.models import WIN
from lolwatch.riot.errors import (
    RiotAuthError,
    RiotBadRequest,
    RiotNotFound,
    RiotRateLimited,
    RiotUnavailable,
)
from lolwatch.riot.http_client import HttpRiotClient, RateLimiter, _split_match_id
from lolwatch.riot.parsing import MatchParseError
from lolwatch.riot.retry import RetryingClient
from lolwatch.riot.routing import PLATFORM_TO_REGION, is_platform, region_for

# -- retry policy ------------------------------------------------------------

class Flaky(FakeRiot):
    def __init__(self, failures):
        super().__init__()
        self.failures = list(failures)
        self.calls = 0

    async def is_in_game(self, puuid, platform):
        self.calls += 1
        if self.failures:
            raise self.failures.pop(0)
        return True


def retrying(inner, sleeps):
    async def sleep(seconds):
        sleeps.append(seconds)
    return RetryingClient(inner, attempts=3, base_delay=2.0, sleep=sleep)


async def test_unavailable_is_retried_with_growing_jittered_backoff():
    sleeps: list[float] = []
    inner = Flaky([RiotUnavailable("503"), RiotUnavailable("503")])
    assert await retrying(inner, sleeps).is_in_game("p", "euw1") is True
    assert inner.calls == 3
    assert len(sleeps) == 2 and 1.0 <= sleeps[0] <= 3.0 and 2.0 <= sleeps[1] <= 6.0


async def test_unavailable_gives_up_after_the_attempt_limit():
    inner = Flaky([RiotUnavailable("x")] * 5)
    with pytest.raises(RiotUnavailable):
        await retrying(inner, []).is_in_game("p", "euw1")
    assert inner.calls == 3


async def test_rate_limit_waits_retry_after_once():
    sleeps: list[float] = []
    inner = Flaky([RiotRateLimited("429", retry_after=7)])
    assert await retrying(inner, sleeps).is_in_game("p", "euw1") is True
    assert sleeps == [7]

    inner = Flaky([RiotRateLimited("429", retry_after=1)] * 2)
    with pytest.raises(RiotRateLimited):
        await retrying(inner, []).is_in_game("p", "euw1")


@pytest.mark.parametrize("error", [RiotAuthError("403"), RiotNotFound("404"),
                                   RiotBadRequest("400")])
async def test_definitive_errors_are_never_retried(error):
    inner = Flaky([error])
    sleeps: list[float] = []
    with pytest.raises(type(error)):
        await retrying(inner, sleeps).is_in_game("p", "euw1")
    assert inner.calls == 1 and sleeps == []


# -- HTTP client -------------------------------------------------------------

class Riot:
    """Mock Riot: routes `host + path` to (status, json, headers) and records requests."""

    def __init__(self, routes):
        self.routes, self.requests = routes, []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        key = f"{request.url.host.split('.')[0]}{request.url.path}"
        status, body, headers = self.routes.get(key, (404, {"status": {}}, {}))
        return httpx.Response(status, json=body, headers=headers)


class NoLimit(RateLimiter):
    async def acquire(self):
        pass


def client(routes):
    riot = Riot(routes)
    return HttpRiotClient("RGAPI-test", transport=httpx.MockTransport(riot),
                          limiter=NoLimit()), riot


ACCOUNT = {"puuid": "puuid-alice", "gameName": "Alice", "tagLine": "EUW"}


async def test_resolve_account_routes_by_region_and_sends_the_key():
    c, riot = client({
        "europe/riot/account/v1/accounts/by-riot-id/Al ice/EUW": (200, ACCOUNT, {}),
        "euw1/lol/summoner/v4/summoners/by-puuid/puuid-alice": (200, {"summonerLevel": 321}, {}),
    })
    acct = await c.resolve_account("Al ice", "EUW", "euw1")
    assert (acct.puuid, acct.game_name, acct.tag_line, acct.summoner_level) == \
        ("puuid-alice", "Alice", "EUW", 321)
    assert riot.requests[0].headers["X-Riot-Token"] == "RGAPI-test"
    assert "Al%20ice" in str(riot.requests[0].url)


async def test_resolve_account_survives_a_failed_level_lookup():
    c, _ = client({"americas/riot/account/v1/accounts/by-riot-id/Bob/NA1": (200, ACCOUNT, {})})
    assert (await c.resolve_account("Bob", "NA1", "na1")).summoner_level is None


async def test_sea_accounts_are_served_from_asia():
    c, _ = client({"asia/riot/account/v1/accounts/by-puuid/p": (200, ACCOUNT, {})})
    assert (await c.account_by_puuid("p", "oc1")).game_name == "Alice"


async def test_account_without_a_riot_id_is_not_found():
    c, _ = client({"europe/riot/account/v1/accounts/by-puuid/p": (200, {"puuid": "p"}, {})})
    with pytest.raises(RiotNotFound):
        await c.account_by_puuid("p", "euw1")


async def test_recent_match_ids_sends_start_time_in_seconds():
    c, riot = client({"europe/lol/match/v5/matches/by-puuid/p/ids":
                      (200, ["EUW1_2", "EUW1_1"], {})})
    since = datetime(2026, 1, 1, tzinfo=UTC)
    assert await c.recent_match_ids("p", "euw1", since, 5) == ["EUW1_2", "EUW1_1"]
    params = riot.requests[0].url.params
    assert params["startTime"] == str(int(since.timestamp()))
    assert (params["start"], params["count"]) == ("0", "5")

    await c.recent_match_ids("p", "euw1", None)
    assert "startTime" not in riot.requests[1].url.params


async def test_get_match_parses_info_and_routes_by_the_match_id():
    c, _ = client({"europe/lol/match/v5/matches/EUW1_7000000001":
                   (200, load_fixture("match_ranked"), {})})
    match = await c.get_match("EUW1_7000000001", "na1")   # the id, not the player, decides
    assert match.participants["puuid-alice"].win == WIN
    assert match.participants["puuid-bob"].champion == "Wukong"


async def test_get_match_without_info_is_unparseable():
    c, _ = client({"europe/lol/match/v5/matches/EUW1_1": (200, {"metadata": {}}, {})})
    with pytest.raises(MatchParseError):
        await c.get_match("EUW1_1", "euw1")


async def test_spectator_200_is_in_game_and_404_is_not():
    path = "euw1/lol/spectator/v5/active-games/by-summoner/"
    c, riot = client({path + "playing": (200, {"gameId": 1}, {})})
    assert await c.is_in_game("playing", "euw1") is True
    assert await c.is_in_game("idle", "euw1") is False
    assert len(riot.requests) == 2               # one call each, no summoner lookup


@pytest.mark.parametrize("code, expected", [
    (401, RiotAuthError), (403, RiotAuthError), (404, RiotNotFound), (400, RiotBadRequest),
    (429, RiotRateLimited), (500, RiotUnavailable), (503, RiotUnavailable),
])
async def test_http_codes_map_to_typed_errors(code, expected):
    c, _ = client({"europe/riot/account/v1/accounts/by-puuid/p": (code, {}, {})})
    with pytest.raises(expected):
        await c.account_by_puuid("p", "euw1")


@pytest.mark.parametrize("kind", ["application", "method", "service", "server", None])
async def test_every_429_is_a_rate_limit_carrying_retry_after(kind):
    headers = {"Retry-After": "11"} | ({"X-Rate-Limit-Type": kind} if kind else {})
    c, _ = client({"europe/riot/account/v1/accounts/by-puuid/p": (429, {}, headers)})
    with pytest.raises(RiotRateLimited) as info:
        await c.account_by_puuid("p", "euw1")
    assert info.value.retry_after == 11.0


async def test_network_failures_are_unavailable():
    def boom(request):
        raise httpx.ConnectTimeout("slow", request=request)
    c = HttpRiotClient("k", transport=httpx.MockTransport(boom), limiter=NoLimit())
    with pytest.raises(RiotUnavailable):
        await c.is_in_game("p", "euw1")


async def test_a_429_pauses_the_limiter():
    paused = []

    class Recording(NoLimit):
        def pause(self, seconds):
            paused.append(seconds)

    riot = Riot({"europe/riot/account/v1/accounts/by-puuid/p": (429, {}, {"Retry-After": "7"})})
    c = HttpRiotClient("k", transport=httpx.MockTransport(riot), limiter=Recording())
    with pytest.raises(RiotRateLimited):
        await c.account_by_puuid("p", "euw1")
    assert paused == [7.0]


# -- rate limiter ------------------------------------------------------------

class FakeTime:
    def __init__(self):
        self.now, self.sleeps = 0.0, []

    def clock(self):
        return self.now

    async def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


async def test_limiter_waits_for_the_tightest_window():
    t = FakeTime()
    limiter = RateLimiter(((2, 1.0), (3, 10.0)), clock=t.clock, sleep=t.sleep)
    for _ in range(3):
        await limiter.acquire()
    assert t.sleeps == [1.0]                     # third call: the 2-per-second window
    await limiter.acquire()
    assert t.sleeps == [1.0, 9.0]                # fourth: the 3-per-10s window
    assert t.now == 10.0


async def test_limiter_pause_holds_every_caller():
    t = FakeTime()
    limiter = RateLimiter(((100, 1.0),), clock=t.clock, sleep=t.sleep)
    limiter.pause(11)
    await limiter.acquire()
    assert t.now == 11


# -- routing -----------------------------------------------------------------

def test_platform_routing_lives_in_one_table():
    assert region_for("EUW1") == "europe" and region_for("na1") == "americas"
    assert region_for("kr") == "asia" and region_for("oc1") == "sea"
    assert is_platform("KR") and not is_platform("mars")
    with pytest.raises(ValueError):
        region_for("mars")
    assert set(PLATFORM_TO_REGION.values()) == {"americas", "europe", "asia", "sea"}


def test_split_match_id():
    assert _split_match_id("EUW1_7000000001") == ("EUW1", 7000000001)
    with pytest.raises(ValueError):
        _split_match_id("garbage")
