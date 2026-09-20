from __future__ import annotations

from datetime import date
from zoneinfo import ZoneInfo

from conftest import MIN, T0, make_match, make_player

from lolwatch.formatting import (
    SPLIT_AT,
    TELEGRAM_LIMIT,
    fmt_date_range,
    fmt_duration,
    format_session,
    format_weekly,
)
from lolwatch.summary import PlayerSummary, QueueStats, Summary

LONDON = ZoneInfo("Europe/London")


def test_fmt_duration():
    assert fmt_duration(30) == "<1m"
    assert fmt_duration(31 * 60) == "31m"
    assert fmt_duration(2 * 3600 + 14 * 60) == "2h 14m"
    assert fmt_duration(3600) == "1h 0m"


def test_single_game_collapses_to_one_line_without_1_games():
    text = format_session(make_player(name="Faker", tag="KR1"),
                          [make_match("a", T0, 31, champion="Azir", kda=(8, 2, 11))])
    assert "\n" not in text
    assert "1 games" not in text and "1 game" not in text
    assert text == "🎮 <b>Faker#KR1</b> — Azir · Ranked Solo · WIN 8/2/11 · 31m"


def test_multi_game_session_has_header_record_and_aligned_table():
    p = make_player(name="Faker", tag="KR1")
    ms = [
        make_match("a", T0, 31, champion="Azir", kda=(8, 2, 11)),
        make_match("b", T0 + 40 * MIN, 28, champion="Azir", win=0, kda=(4, 7, 6)),
        make_match("c", T0 + 80 * MIN, 42, champion="Orianna", kda=(11, 3, 9)),
        make_match("d", T0 + 130 * MIN, 22, champion="Ziggs", queue_id=450, kda=(19, 8, 22)),
    ]
    text = format_session(p, ms)
    head, stats, _, *_ = text.split("\n")
    assert head == "🎮 <b>Faker#KR1</b> — session over"
    assert stats == "4 games · 2h 3m · 3W 1L"
    assert text.count("<pre>") == 1 and text.endswith("</pre>")
    rows = text.split("<pre>")[1].split("</pre>")[0].splitlines()
    assert len(rows) == 4
    assert len({len(r) for r in rows}) == 1                # columns are aligned
    assert "Ziggs" in rows[3] and "ARAM" in rows[3] and "19/8/22" in rows[3]


def test_session_orders_games_chronologically_even_if_given_shuffled():
    p = make_player()
    late, early = make_match("late", T0 + 60 * MIN, champion="Ahri"), \
        make_match("early", T0, champion="Zed")
    rows = format_session(p, [late, early]).split("<pre>")[1].splitlines()
    assert "Zed" in rows[0] and "Ahri" in rows[1]


def test_remakes_are_counted_separately_and_html_is_escaped():
    p = make_player(name="A<b>&", tag="X1")
    text = format_session(p, [make_match("a", T0, 3, remake=True, win=0),
                              make_match("b", T0 + 10 * MIN, 30)])
    assert "1W 0L · 1 remake" in text
    assert "REMAKE" in text
    assert "A&lt;b&gt;&amp;#X1" in text and "A<b>&" not in text


def test_arena_result_renders_as_dash_not_win():
    text = format_session(make_player(), [make_match("a", T0, 23, queue_id=1700, win=-1)])
    assert "Arena" in text and "WIN" not in text and "LOSS" not in text


def test_date_range():
    assert fmt_date_range(date(2026, 9, 14), date(2026, 9, 20)) == "14–20 Sep"
    assert fmt_date_range(date(2026, 9, 28), date(2026, 10, 4)) == "28 Sep – 4 Oct"
    assert fmt_date_range(date(2026, 9, 14), date(2026, 9, 14)) == "14 Sep"


def _summary(riot_id="Faker#KR1", **kw) -> PlayerSummary:
    base = dict(
        riot_id=riot_id, games=23, seconds=11 * 3600 + 42 * 60, wins=10, losses=6, remakes=1,
        groups=(QueueStats("Ranked Solo", 16, 8 * 3600 + 31 * 60, 10, 6),
                QueueStats("ARAM", 6, 2 * 3600 + 48 * 60, 3, 3),
                QueueStats("Arena", 1, 23 * 60, 0, 0)),
        top_champions=(("Azir", 7), ("Orianna", 4)))
    base.update(kw)
    return PlayerSummary(**base)


def test_weekly_message_matches_spec_shape():
    s = Summary(date(2026, 9, 14), date(2026, 9, 20), (_summary(),))
    [text] = format_weekly(s, LONDON)
    assert text.startswith("📊 <b>Week of 14–20 Sep</b>")
    assert "<b>Faker#KR1</b>" in text
    assert "23 games · 11h 42m · 63% WR" in text
    assert "Ranked Solo" in text and "16 games · 8h 31m · 63% (10W 6L)" in text
    arena_line = next(line for line in text.splitlines() if "Arena" in line)
    assert "1 game " in arena_line and "23m" in arena_line
    assert "%" not in arena_line                     # no winrate for placement modes
    assert "Most played: Azir (7), Orianna (4)" in text
    assert "(1 remake excluded from winrate)" in text


def test_weekly_player_with_no_games_and_no_players():
    s = Summary(date(2026, 9, 14), date(2026, 9, 20), (PlayerSummary(riot_id="Idle#EUW"),))
    assert "no games" in format_weekly(s, LONDON)[0]
    empty = Summary(date(2026, 9, 14), date(2026, 9, 20), ())
    assert "No tracked players" in format_weekly(empty, LONDON)[0]


def test_weekly_splits_across_messages_under_the_telegram_limit():
    players = tuple(_summary(riot_id=f"Player{i:02d}#EUW") for i in range(40))
    messages = format_weekly(Summary(date(2026, 9, 14), date(2026, 9, 20), players), LONDON)
    assert len(messages) > 1
    assert all(len(m) <= SPLIT_AT < TELEGRAM_LIMIT for m in messages)
    joined = "\n".join(messages)
    assert all(f"Player{i:02d}#EUW" in joined for i in range(40))
    assert all(m.count("<pre>") == m.count("</pre>") for m in messages)  # no torn HTML
