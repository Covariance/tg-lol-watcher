"""match-v5 parsing against fixture JSON, including the spec's known traps (§6).

The fixtures are hand-built to match the match-v5 `info` shape (no live recording
was possible without an API key); they cover each trap deliberately.
"""

from __future__ import annotations

import pytest
from conftest import load_fixture

from lolwatch.models import LOSS, UNKNOWN, WIN
from lolwatch.riot.parsing import MatchParseError, derive_times, parse_match


def parsed(name: str):
    raw = load_fixture(name)
    return parse_match(raw["metadata"]["matchId"], raw["info"])


def test_ranked_match_uses_end_minus_start_and_maps_participants():
    m = parsed("match_ranked")
    assert (m.queue_id, m.duration_s, m.remake) == (420, 1860, False)
    assert m.end_ms - m.start_ms == 1_860_000
    alice = m.participants["puuid-alice"]
    assert (alice.champion, alice.win, alice.kills, alice.deaths, alice.assists) == \
        ("Azir", WIN, 8, 2, 11)
    assert m.participants["puuid-bob"].champion == "Wukong"      # MonkeyKing display name
    assert m.participants["puuid-x1"].win == LOSS                 # opponents are parsed too
    assert len(m.participants) == 3


def test_pre_patch_11_20_millisecond_duration_is_corrected():
    m = parsed("match_old_ms")
    assert m.duration_s == 1500                       # 1_500_000 "ms" -> 25 minutes
    assert m.end_ms == m.start_ms + 1_500_000


def test_remake_is_flagged_by_early_surrender_and_short_duration():
    m = parsed("match_remake")
    assert m.remake
    raw = load_fixture("match_remake")["info"]
    raw["participants"][0]["gameEndedInEarlySurrender"] = False
    assert parse_match("x", raw).remake               # 200s is under 5 minutes anyway


def test_arena_result_is_unknown_rather_than_a_misleading_win():
    m = parsed("match_arena")
    assert m.queue_id == 1700
    assert m.participants["puuid-alice"].win == UNKNOWN
    assert not m.remake


def test_bot_participants_are_skipped():
    m = parsed("match_bots")
    assert list(m.participants) == ["puuid-alice"]


def test_absurd_duration_is_a_parsing_error_not_a_long_game():
    with pytest.raises(MatchParseError):
        parsed("match_absurd")


def test_missing_timestamps_raise():
    with pytest.raises(MatchParseError):
        parse_match("x", {"queueId": 420, "participants": []})


def test_loss_and_missing_stats_are_tolerated():
    info = load_fixture("match_ranked")["info"]
    info["participants"][0].update(win=False, kills=None)
    del info["participants"][0]["assists"]
    p = parse_match("x", info).participants["puuid-alice"]
    assert p.win == LOSS and p.kills is None and p.assists is None


def test_derive_times_falls_back_to_duration_when_end_is_insane():
    info = {"gameStartTimestamp": 1_000_000, "gameEndTimestamp": 1_000_000 + 999_999_999,
            "gameDuration": 600}
    assert derive_times(info) == (1_000_000, 1_600_000, 600)
