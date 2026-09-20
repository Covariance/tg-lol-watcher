from __future__ import annotations

from datetime import datetime
from zoneinfo import ZoneInfo

import pytest
from conftest import MIN, T0, make_match

from lolwatch.summary import build_summary, summarize_player, window_days

GAP = 15 * MIN
LONDON = ZoneInfo("Europe/London")


def test_no_matches_is_an_empty_summary():
    s = summarize_player("A#1", [], GAP)
    assert s.games == 0 and s.winrate is None and s.groups == ()


def test_winrate_excludes_remakes_and_unknown_results():
    ms = [
        make_match("1", T0, 30, win=1),
        make_match("2", T0 + 60 * MIN, 30, win=0),
        make_match("3", T0 + 120 * MIN, 30, win=1),
        make_match("4", T0 + 180 * MIN, 3, win=0, remake=True),
        make_match("5", T0 + 240 * MIN, 25, queue_id=1700, win=-1),
    ]
    s = summarize_player("A#1", ms, GAP)
    assert s.games == 5                        # remakes and arena still count as games...
    assert (s.wins, s.losses, s.remakes) == (2, 1, 1)
    assert s.winrate == 67                     # ...but only decided games enter the winrate
    assert s.seconds == (30 * 3 + 3 + 25) * 60


def test_winrate_rounds_half_up():
    ms = [make_match(str(i), T0 + i * 60 * MIN, win=1 if i < 10 else 0) for i in range(16)]
    assert summarize_player("A#1", ms, GAP).winrate == 63          # 62.5 -> 63, not 62


def test_groups_are_split_by_queue_and_ordered():
    ms = [
        make_match("a", T0, queue_id=450, champion="Ziggs"),
        make_match("b", T0 + 60 * MIN, queue_id=420),
        make_match("c", T0 + 120 * MIN, queue_id=420),
        make_match("d", T0 + 180 * MIN, queue_id=1700, win=-1),
        make_match("e", T0 + 240 * MIN, queue_id=9999),
    ]
    groups = summarize_player("A#1", ms, GAP).groups
    assert [(g.group, g.games) for g in groups] == [
        ("Ranked Solo", 2), ("ARAM", 1), ("Arena", 1), ("Queue 9999", 1)]
    assert groups[2].winrate is None            # Arena: no decided games -> no winrate


def test_most_played_champions_are_deterministic():
    ms = [make_match(str(i), T0 + i * 60 * MIN, champion=c)
          for i, c in enumerate(["Azir", "Ahri", "Azir", "Zed", "Ahri", "Azir", "Lux"])]
    assert summarize_player("A#1", ms, GAP).top_champions == \
        (("Azir", 3), ("Ahri", 2), ("Lux", 1))


def test_longest_session_uses_the_gap_rule():
    ms = [make_match("a", T0), make_match("b", T0 + 35 * MIN), make_match("c", T0 + 70 * MIN),
          make_match("d", T0 + 500 * MIN)]
    longest = summarize_player("A#1", ms, GAP).longest_session
    assert longest.games == 3 and longest.started_ms == T0
    solo = summarize_player("A#1", [make_match("a", T0)], GAP)
    assert solo.longest_session is None        # a lone game isn't a "session" worth reporting


def test_window_days_is_seven_local_days_from_monday_midnight():
    now = datetime(2026, 9, 20, 20, 0, tzinfo=LONDON)        # Sunday 20:00
    start, end = window_days(now, 7)
    assert start == datetime(2026, 9, 14, 0, 0, tzinfo=LONDON)   # Monday 00:00
    assert end == now


def test_window_start_is_local_midnight_across_a_dst_change():
    now = datetime(2026, 3, 30, 12, 0, tzinfo=LONDON)         # UK clocks went forward 29 Mar
    start, _ = window_days(now, 3)
    assert (start.year, start.month, start.day, start.hour) == (2026, 3, 28, 0)
    assert start.utcoffset() != now.utcoffset()               # GMT vs BST: still midnight local


def test_window_requires_an_aware_datetime():
    with pytest.raises(ValueError):
        window_days(datetime(2026, 9, 20, 20, 0), 7)


def test_build_summary_dates_are_local():
    start = datetime(2026, 9, 14, 0, 0, tzinfo=LONDON)
    end = datetime(2026, 9, 20, 20, 0, tzinfo=LONDON)
    s = build_summary(start, end, LONDON, [("A#1", [make_match("a", T0)])], GAP)
    assert (s.start.day, s.end.day) == (14, 20) and s.players[0].games == 1
