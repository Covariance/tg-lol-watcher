"""Riot adapter internals that can be verified offline: retry policy, error mapping,
and the Cassiopeia -> match-v5 translation. No network."""

from __future__ import annotations

import pytest
from conftest import FakeRiot, load_fixture

from lolwatch.models import WIN
from lolwatch.riot.cassiopeia_impl import _split_match_id, core_to_info, map_exception
from lolwatch.riot.errors import (
    RiotAuthError,
    RiotBadRequest,
    RiotNotFound,
    RiotRateLimited,
    RiotUnavailable,
)
from lolwatch.riot.parsing import parse_match
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


# -- error mapping -----------------------------------------------------------

class HttpError(Exception):
    def __init__(self, code, headers=None):
        super().__init__(f"HTTP {code}")
        self.code = code
        self.response_headers = headers or {}


@pytest.mark.parametrize("code, expected", [
    (401, RiotAuthError), (403, RiotAuthError), (404, RiotNotFound), (400, RiotBadRequest),
    (429, RiotRateLimited), (500, RiotUnavailable), (503, RiotUnavailable),
])
def test_http_codes_map_to_typed_errors(code, expected):
    assert isinstance(map_exception(HttpError(code)), expected)


def test_403_is_auth_even_though_cassiopeia_names_it_a_request_error():
    class APIRequestError(HttpError):    # Cassiopeia's actual class for 403
        pass
    assert isinstance(map_exception(APIRequestError(403)), RiotAuthError)


def test_exhausted_429_arrives_as_a_bare_runtimeerror_in_cassiopeia():
    mapped = map_exception(RuntimeError('Encountered an HTTP error code 429 with message "x"'))
    assert isinstance(mapped, RiotRateLimited)


def test_retry_after_header_is_carried():
    mapped = map_exception(HttpError(429, {"Retry-After": "12"}))
    assert mapped.retry_after == 12.0


def test_pipeline_not_found_and_network_failures():
    class NotFoundError(Exception):
        pass
    assert isinstance(map_exception(NotFoundError("no such summoner")), RiotNotFound)
    assert isinstance(map_exception(TimeoutError("slow")), RiotUnavailable)
    assert isinstance(map_exception(ConnectionResetError("reset")), RiotUnavailable)
    assert map_exception(ValueError("a genuine bug")) is None      # bugs must not be masked


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


# -- Cassiopeia core-data translation ---------------------------------------

def test_cassiopeia_core_data_round_trips_through_the_shared_parser():
    """Feed a fixture through Cassiopeia's own MatchData constructor, rebuild the
    info dict from it, and check we recover the same result as parsing directly.
    Guards the `_data` reach-in in cassiopeia_impl against upstream drift."""
    from cassiopeia.core.match import MatchData as CoreMatchData

    raw = load_fixture("match_ranked")["info"]
    core_kwargs = {**raw, "platformId": "EUW1", "gameId": 1, "teams": [
        {"teamId": 100, "win": True, "bans": [], "objectives": {}}]}
    rebuilt = core_to_info(CoreMatchData(**core_kwargs))
    via_cassiopeia = parse_match("EUW1_7000000001", rebuilt)
    direct = parse_match("EUW1_7000000001", raw)

    assert via_cassiopeia == direct
    assert via_cassiopeia.participants["puuid-alice"].win == WIN
    assert via_cassiopeia.participants["puuid-alice"].kills == 8
    assert via_cassiopeia.participants["puuid-bob"].champion == "Wukong"


def test_cassiopeia_remake_flag_survives_the_translation():
    from cassiopeia.core.match import MatchData as CoreMatchData

    raw = load_fixture("match_remake")["info"]
    core = CoreMatchData(**{**raw, "platformId": "EUW1", "gameId": 1, "teams": [
        {"teamId": 100, "win": False, "bans": [], "objectives": {}}]})
    assert parse_match("x", core_to_info(core)).remake


def test_cassiopeia_http_calls_get_a_timeout(monkeypatch):
    """Its requests-based fallback sets none, so a hung connection would freeze polling."""
    import requests
    from cassiopeia.datastores import common

    from lolwatch.riot.cassiopeia_impl import HTTP_TIMEOUT_S

    if common.USE_PYCURL:
        pytest.skip("pycurl client in use")
    seen = {}
    monkeypatch.setattr(requests, "get", lambda url, **kw: seen.update(kw))
    common.requests.get("https://example.invalid", headers={"a": "b"})
    assert seen["timeout"] == HTTP_TIMEOUT_S and seen["headers"] == {"a": "b"}
    assert common.requests.Session is requests.Session      # everything else passes through
