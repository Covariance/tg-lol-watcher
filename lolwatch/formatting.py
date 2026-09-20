"""Message assembly (Telegram HTML). Pure string functions, tested on their own."""

from __future__ import annotations

from datetime import date, datetime, tzinfo
from html import escape

from .models import LOSS, WIN, Match, Player
from .queues import queue_group
from .summary import PlayerSummary, Summary

TELEGRAM_LIMIT = 4096
SPLIT_AT = 3800   # leave headroom under the hard limit for tags/entities


def plural(n: int, noun: str) -> str:
    return f"{n} {noun}" if n == 1 else f"{n} {noun}s"


def fmt_duration(seconds: int) -> str:
    if seconds < 60:
        return "<1m"
    hours, minutes = divmod((seconds + 30) // 60, 60)   # half-up, not banker's rounding
    return f"{hours}h {minutes}m" if hours else f"{minutes}m"


def fmt_result(m: Match) -> str:
    if m.remake:
        return "REMAKE"
    return {WIN: "WIN", LOSS: "LOSS"}.get(m.win, "—")


def fmt_kda(m: Match) -> str:
    if m.kills is None or m.deaths is None or m.assists is None:
        return "-"
    return f"{m.kills}/{m.deaths}/{m.assists}"


def record(matches: list[Match]) -> str:
    """`3W 1L`, plus remake / unknown counts when present."""
    wins = sum(1 for m in matches if not m.remake and m.win == WIN)
    losses = sum(1 for m in matches if not m.remake and m.win == LOSS)
    parts = []
    if wins or losses:
        parts.append(f"{wins}W {losses}L")
    remakes = sum(1 for m in matches if m.remake)
    if remakes:
        parts.append(plural(remakes, "remake"))
    return " · ".join(parts)


def format_game(player: Player, m: Match) -> str:
    """Single-game line (also used for `per_game` mode)."""
    return (f"🎮 <b>{escape(player.riot_id)}</b> — {escape(m.champion)} · "
            f"{escape(queue_group(m.queue_id))} · {fmt_result(m)} {fmt_kda(m)} · "
            f"{fmt_duration(m.duration_s)}")


def format_session(player: Player, matches: list[Match]) -> str:
    """One notification for a finished session. Single game collapses to a line."""
    matches = sorted(matches, key=lambda m: m.start_ms)
    if len(matches) == 1:
        return format_game(player, matches[0])

    total = sum(m.duration_s for m in matches)
    header = [plural(len(matches), "game"), fmt_duration(total)]
    rec = record(matches)
    if rec:
        header.append(rec)

    rows = [(m.champion, queue_group(m.queue_id), fmt_result(m), fmt_kda(m),
             fmt_duration(m.duration_s)) for m in matches]

    def render(shown: list[tuple[str, ...]]) -> str:
        widths = [max(len(r[i]) for r in shown) for i in range(5)]
        table = "\n".join(
            f"  {c:<{widths[0]}}  {q:<{widths[1]}}  {res:<{widths[2]}}  "
            f"{k:<{widths[3]}}  {d:>{widths[4]}}"
            for c, q, res, k, d in shown)
        hidden = len(rows) - len(shown)
        note = f"\n(+{plural(hidden, 'earlier game')} not shown)" if hidden else ""
        return (f"🎮 <b>{escape(player.riot_id)}</b> — session over\n"
                f"{' · '.join(header)}\n\n<pre>{escape(table)}</pre>{note}")

    # A marathon must not exceed Telegram's limit: cutting the text afterwards would
    # slice through the <pre> tag and Telegram would reject the whole message.
    shown = rows
    while len(render(shown)) > SPLIT_AT and len(shown) > 1:
        shown = shown[1:]
    return render(shown)


# -- weekly ------------------------------------------------------------------

def fmt_date_range(start: date, end: date) -> str:
    if start == end:
        return f"{start.day} {start:%b}"
    if (start.year, start.month) == (end.year, end.month):
        return f"{start.day}–{end.day} {end:%b}"
    return f"{start.day} {start:%b} – {end.day} {end:%b}"


def _part_of_day(hour: int) -> str:
    if 5 <= hour < 12:
        return "morning"
    if 12 <= hour < 17:
        return "afternoon"
    if 17 <= hour < 22:
        return "evening"
    return "night"


def format_player_summary(ps: PlayerSummary, tz: tzinfo) -> str:
    if ps.games == 0:
        return f"<b>{escape(ps.riot_id)}</b>\n<pre>  no games</pre>"

    head = f"{plural(ps.games, 'game')} · {fmt_duration(ps.seconds)}"
    if ps.winrate is not None:
        head += f" · {ps.winrate}% WR"
    lines = [f"  {head}"]

    name_w = max(len(g.group) for g in ps.groups)
    games_w = max(len(plural(g.games, "game")) for g in ps.groups)
    for g in ps.groups:
        line = f"    {g.group:<{name_w}}  {plural(g.games, 'game'):<{games_w}} · {fmt_duration(g.seconds)}"
        if g.winrate is not None:
            line += f" · {g.winrate}% ({g.wins}W {g.losses}L)"
        lines.append(line)

    if ps.top_champions:
        lines.append("  Most played: " + ", ".join(f"{c} ({n})" for c, n in ps.top_champions))
    if ps.longest_session:
        when = datetime.fromtimestamp(ps.longest_session.started_ms / 1000, tz)
        lines.append(f"  Longest session: {plural(ps.longest_session.games, 'game')}, "
                     f"{when:%a} {_part_of_day(when.hour)}")
    if ps.remakes:
        lines.append(f"  ({plural(ps.remakes, 'remake')} excluded from winrate)")
    return f"<b>{escape(ps.riot_id)}</b>\n<pre>{escape(chr(10).join(lines))}</pre>"


def window_title(summary: Summary, days: int = 7, rolling: bool = False) -> str:
    if rolling:
        return f"Week ending {summary.end.day} {summary.end:%b}"
    if days == 7:
        return f"Week of {fmt_date_range(summary.start, summary.end)}"
    return f"Last {plural(days, 'day')} · {fmt_date_range(summary.start, summary.end)}"


def format_weekly(summary: Summary, tz: tzinfo, limit: int = SPLIT_AT, *,
                  days: int = 7, rolling: bool = False) -> list[str]:
    """One or more messages, each under the Telegram limit, split between players."""
    title = f"📊 <b>{window_title(summary, days, rolling)}</b>"
    if not summary.players:
        return [f"{title}\n\nNo tracked players."]

    blocks = [format_player_summary(p, tz) for p in summary.players]
    messages: list[str] = []
    current = title
    for block in blocks:
        # A single player block is bounded (one line per queue group), so it
        # always fits in one message on its own; only whole blocks are moved.
        if len(current) + len(block) + 2 > limit:
            messages.append(current)
            current = block
        else:
            current = f"{current}\n\n{block}"
    messages.append(current)
    return messages
