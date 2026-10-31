//! Message assembly (Telegram HTML). Pure string functions, tested on their own.
//!
//! Lengths and column widths count chars (code points), as Telegram's limit does
//! for everything we send.

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::models::{LOSS, Match, Player, WIN, queue_group};
use crate::summary::{PlayerSummary, Summary};

pub const TELEGRAM_LIMIT: usize = 4096;
/// Leave headroom under the hard limit for tags/entities.
pub const SPLIT_AT: usize = 3800;

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// HTML escaping exactly like Python's `html.escape(s, quote=True)`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

pub fn chars(s: &str) -> usize {
    s.chars().count()
}

pub fn plural(n: i64, noun: &str) -> String {
    if n == 1 { format!("{n} {noun}") } else { format!("{n} {noun}s") }
}

pub fn fmt_duration(seconds: i64) -> String {
    if seconds < 60 {
        return "<1m".into();
    }
    let minutes = (seconds + 30) / 60; // half-up, not banker's rounding
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours > 0 { format!("{hours}h {minutes}m") } else { format!("{minutes}m") }
}

fn fmt_result(m: &Match) -> &'static str {
    if m.remake {
        "REMAKE"
    } else if m.win == WIN {
        "WIN"
    } else if m.win == LOSS {
        "LOSS"
    } else {
        "—"
    }
}

fn fmt_kda(m: &Match) -> String {
    match (m.kills, m.deaths, m.assists) {
        (Some(k), Some(d), Some(a)) => format!("{k}/{d}/{a}"),
        _ => "-".into(),
    }
}

/// `3W 1L`, plus a remake count when present.
fn record(matches: &[&Match]) -> String {
    let wins = matches.iter().filter(|m| m.is_win()).count() as i64;
    let losses = matches.iter().filter(|m| m.is_loss()).count() as i64;
    let remakes = matches.iter().filter(|m| m.remake).count() as i64;
    let mut parts = Vec::new();
    if wins > 0 || losses > 0 {
        parts.push(format!("{wins}W {losses}L"));
    }
    if remakes > 0 {
        parts.push(plural(remakes, "remake"));
    }
    parts.join(" · ")
}

/// Single-game line (also used for `per_game` mode).
pub fn format_game(player: &Player, m: &Match) -> String {
    format!(
        "🎮 <b>{}</b> — {} · {} · {} {} · {}",
        escape(&player.riot_id()),
        escape(&m.champion),
        escape(&queue_group(m.queue_id)),
        fmt_result(m),
        fmt_kda(m),
        fmt_duration(m.duration_s)
    )
}

/// Left-pad-free column write: `text` then spaces up to `width` chars.
fn pad_right(out: &mut String, text: &str, width: usize) {
    out.push_str(text);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(chars(text))));
}

/// One notification for a finished session. A single game collapses to a line.
pub fn format_session(player: &Player, matches: &[Match]) -> String {
    let mut ordered: Vec<&Match> = matches.iter().collect();
    ordered.sort_by_key(|m| m.start_ms);
    if ordered.len() == 1 {
        return format_game(player, ordered[0]);
    }

    let total = ordered.iter().map(|m| m.duration_s).sum();
    let mut header = vec![plural(ordered.len() as i64, "game"), fmt_duration(total)];
    let rec = record(&ordered);
    if !rec.is_empty() {
        header.push(rec);
    }
    let header = header.join(" · ");
    let title = format!("🎮 <b>{}</b> — session over\n{header}\n\n<pre>", escape(&player.riot_id()));

    let rows: Vec<[String; 5]> = ordered
        .iter()
        .map(|m| {
            [m.champion.clone(), queue_group(m.queue_id), fmt_result(m).into(), fmt_kda(m), fmt_duration(m.duration_s)]
        })
        .collect();

    let render = |shown: &[[String; 5]]| -> String {
        let mut widths = [0usize; 5];
        for row in shown {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(chars(cell));
            }
        }
        let mut table = String::new();
        for (i, [c, q, res, k, d]) in shown.iter().enumerate() {
            if i > 0 {
                table.push('\n');
            }
            table.push_str("  ");
            for (cell, w) in [(c, widths[0]), (q, widths[1]), (res, widths[2]), (k, widths[3])] {
                pad_right(&mut table, cell, w);
                table.push_str("  ");
            }
            table.extend(std::iter::repeat_n(' ', widths[4].saturating_sub(chars(d))));
            table.push_str(d);
        }
        let hidden = (rows.len() - shown.len()) as i64;
        let note =
            if hidden > 0 { format!("\n(+{} not shown)", plural(hidden, "earlier game")) } else { String::new() };
        format!("{title}{}</pre>{note}", escape(&table))
    };

    // A marathon must not exceed Telegram's limit: cutting the text afterwards would
    // slice through the <pre> tag and Telegram would reject the whole message.
    let mut shown = &rows[..];
    loop {
        let text = render(shown);
        if chars(&text) <= SPLIT_AT || shown.len() <= 1 {
            return text;
        }
        shown = &shown[1..];
    }
}

/// Split a message into parts of at most `limit` chars at line breaks. Only for text
/// whose tags open and close on one line (a lone over-long line is cut by chars).
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    if chars(text) <= limit {
        return vec![text.to_owned()];
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for line in text.split('\n') {
        let mut line_len = chars(line);
        let mut line = line;
        let cut;
        if line_len > limit {
            cut = line.chars().take(limit - 1).chain(std::iter::once('…')).collect::<String>();
            line = &cut;
            line_len = limit;
        }
        if current_len > 0 && current_len + 1 + line_len > limit {
            parts.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if current_len > 0 {
            current.push('\n');
            current_len += 1;
        }
        current.push_str(line);
        current_len += line_len;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

// -- weekly -------------------------------------------------------------------

fn month(d: Date) -> &'static str {
    MONTHS[d.month() as usize - 1]
}

pub fn fmt_date_range(start: Date, end: Date) -> String {
    if start == end {
        format!("{} {}", start.day(), month(start))
    } else if (start.year(), start.month()) == (end.year(), end.month()) {
        format!("{}–{} {}", start.day(), end.day(), month(end))
    } else {
        format!("{} {} – {} {}", start.day(), month(start), end.day(), month(end))
    }
}

fn part_of_day(hour: i8) -> &'static str {
    match hour {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=21 => "evening",
        _ => "night",
    }
}

pub fn format_player_summary(ps: &PlayerSummary, tz: &TimeZone) -> String {
    if ps.games == 0 {
        return format!("<b>{}</b>\n<pre>  no games</pre>", escape(&ps.riot_id));
    }

    let mut head = format!("{} · {}", plural(ps.games, "game"), fmt_duration(ps.seconds));
    if let Some(wr) = ps.winrate() {
        let _ = write!(head, " · {wr}% WR");
    }
    let mut lines = vec![format!("  {head}")];

    let name_w = ps.groups.iter().map(|g| chars(&g.group)).max().unwrap_or(0);
    let games_w = ps.groups.iter().map(|g| chars(&plural(g.games, "game"))).max().unwrap_or(0);
    for g in &ps.groups {
        let mut line = String::from("    ");
        pad_right(&mut line, &g.group, name_w);
        line.push_str("  ");
        pad_right(&mut line, &plural(g.games, "game"), games_w);
        let _ = write!(line, " · {}", fmt_duration(g.seconds));
        if let Some(wr) = g.winrate() {
            let _ = write!(line, " · {wr}% ({}W {}L)", g.wins, g.losses);
        }
        lines.push(line);
    }

    if !ps.top_champions.is_empty() {
        let champs: Vec<String> = ps.top_champions.iter().map(|(c, n)| format!("{c} ({n})")).collect();
        lines.push(format!("  Most played: {}", champs.join(", ")));
    }
    if let Some(longest) = ps.longest_session {
        let when = Timestamp::from_millisecond(longest.started_ms).expect("timestamp in range").to_zoned(tz.clone());
        lines.push(format!(
            "  Longest session: {}, {} {}",
            plural(longest.games, "game"),
            when.strftime("%a"),
            part_of_day(when.hour())
        ));
    }
    if ps.remakes > 0 {
        lines.push(format!("  ({} excluded from winrate)", plural(ps.remakes, "remake")));
    }
    format!("<b>{}</b>\n<pre>{}</pre>", escape(&ps.riot_id), escape(&lines.join("\n")))
}

pub fn window_title(summary: &Summary, days: i64, rolling: bool) -> String {
    if rolling {
        format!("Week ending {} {}", summary.end.day(), month(summary.end))
    } else if days == 7 {
        format!("Week of {}", fmt_date_range(summary.start, summary.end))
    } else {
        format!("Last {} · {}", plural(days, "day"), fmt_date_range(summary.start, summary.end))
    }
}

/// One or more messages, each under `limit`, split between players.
pub fn format_weekly(summary: &Summary, tz: &TimeZone, limit: usize, days: i64, rolling: bool) -> Vec<String> {
    let title = format!("📊 <b>{}</b>", window_title(summary, days, rolling));
    if summary.players.is_empty() {
        return vec![format!("{title}\n\nNo tracked players.")];
    }
    let mut messages = Vec::new();
    let mut current = title;
    for player in &summary.players {
        let block = format_player_summary(player, tz);
        // A single player block is bounded (one line per queue group), so it always
        // fits in one message on its own; only whole blocks are moved.
        if chars(&current) + chars(&block) + 2 > limit {
            messages.push(std::mem::replace(&mut current, block));
        } else {
            current.push_str("\n\n");
            current.push_str(&block);
        }
    }
    messages.push(current);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::QueueStats;
    use crate::testutil::{MIN, MatchExt, T0, london, make_match, make_match_with, make_player_named};

    fn date(y: i16, m: i8, d: i8) -> Date {
        Date::new(y, m, d).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(30), "<1m");
        assert_eq!(fmt_duration(31 * 60), "31m");
        assert_eq!(fmt_duration(2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(fmt_duration(3600), "1h 0m");
    }

    #[test]
    fn escape_matches_python() {
        assert_eq!(escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;");
    }

    #[test]
    fn single_game_collapses_to_one_line_without_1_games() {
        let p = make_player_named("Faker", "KR1");
        let text = format_session(&p, &[make_match("a", T0, 31).with_champion("Azir").with_kda(8, 2, 11)]);
        assert_eq!(text, "🎮 <b>Faker#KR1</b> — Azir · Ranked Solo · WIN 8/2/11 · 31m");
    }

    #[test]
    fn multi_game_session_has_header_record_and_aligned_table() {
        let p = make_player_named("Faker", "KR1");
        let ms = [
            make_match("a", T0, 31).with_champion("Azir").with_kda(8, 2, 11),
            make_match_with("b", T0 + 40 * MIN, 28, |m| m.win = 0).with_champion("Azir").with_kda(4, 7, 6),
            make_match("c", T0 + 80 * MIN, 42).with_champion("Orianna").with_kda(11, 3, 9),
            make_match_with("d", T0 + 130 * MIN, 22, |m| m.queue_id = 450).with_champion("Ziggs").with_kda(19, 8, 22),
        ];
        let text = format_session(&p, &ms);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "🎮 <b>Faker#KR1</b> — session over");
        assert_eq!(lines[1], "4 games · 2h 3m · 3W 1L");
        assert_eq!(text.matches("<pre>").count(), 1);
        assert!(text.ends_with("</pre>"));
        let table = text.split("<pre>").nth(1).unwrap().split("</pre>").next().unwrap();
        let rows: Vec<&str> = table.lines().collect();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| chars(r) == chars(rows[0]))); // columns are aligned
        assert!(rows[3].contains("Ziggs") && rows[3].contains("ARAM") && rows[3].contains("19/8/22"));
    }

    #[test]
    fn session_orders_games_chronologically_even_if_given_shuffled() {
        let p = make_player_named("Alice", "EUW");
        let text = format_session(
            &p,
            &[
                make_match("late", T0 + 60 * MIN, 30).with_champion("Ahri"),
                make_match("early", T0, 30).with_champion("Zed"),
            ],
        );
        let rows: Vec<&str> = text.split("<pre>").nth(1).unwrap().lines().collect();
        assert!(rows[0].contains("Zed") && rows[1].contains("Ahri"));
    }

    #[test]
    fn remakes_are_counted_separately_and_html_is_escaped() {
        let p = make_player_named("A<b>&", "X1");
        let text = format_session(
            &p,
            &[
                make_match_with("a", T0, 3, |m| {
                    m.remake = true;
                    m.win = 0;
                }),
                make_match("b", T0 + 10 * MIN, 30),
            ],
        );
        assert!(text.contains("1W 0L · 1 remake") && text.contains("REMAKE"));
        assert!(text.contains("A&lt;b&gt;&amp;#X1") && !text.contains("A<b>&"));
    }

    #[test]
    fn arena_result_renders_as_dash_not_win() {
        let p = make_player_named("Alice", "EUW");
        let text = format_session(
            &p,
            &[make_match_with("a", T0, 23, |m| {
                m.queue_id = 1700;
                m.win = -1;
            })],
        );
        assert!(text.contains("Arena") && !text.contains("WIN") && !text.contains("LOSS"));
    }

    #[test]
    fn long_messages_split_on_line_breaks_under_the_limit() {
        assert_eq!(split_message("short", 10), ["short"]);
        let lines: Vec<String> =
            (0..300).map(|i| format!("• <b>Player{i:03}#EUW</b> (euw1) — last game 2h ago")).collect();
        let text = lines.join("\n");
        let parts = split_message(&text, SPLIT_AT);
        assert!(parts.len() > 1 && parts.iter().all(|p| chars(p) <= SPLIT_AT));
        assert_eq!(parts.join("\n"), text); // nothing lost, nothing torn mid-line
        let giant = "é".repeat(50);
        assert!(split_message(&giant, 10).iter().all(|p| chars(p) <= 10));
    }

    #[test]
    fn date_ranges() {
        assert_eq!(fmt_date_range(date(2026, 9, 14), date(2026, 9, 20)), "14–20 Sep");
        assert_eq!(fmt_date_range(date(2026, 9, 28), date(2026, 10, 4)), "28 Sep – 4 Oct");
        assert_eq!(fmt_date_range(date(2026, 9, 14), date(2026, 9, 14)), "14 Sep");
    }

    fn summary_for(riot_id: &str) -> PlayerSummary {
        PlayerSummary {
            riot_id: riot_id.into(),
            games: 23,
            seconds: 11 * 3600 + 42 * 60,
            wins: 10,
            losses: 6,
            remakes: 1,
            groups: vec![
                QueueStats { group: "Ranked Solo".into(), games: 16, seconds: 8 * 3600 + 31 * 60, wins: 10, losses: 6 },
                QueueStats { group: "ARAM".into(), games: 6, seconds: 2 * 3600 + 48 * 60, wins: 3, losses: 3 },
                QueueStats { group: "Arena".into(), games: 1, seconds: 23 * 60, wins: 0, losses: 0 },
            ],
            top_champions: vec![("Azir".into(), 7), ("Orianna".into(), 4)],
            longest_session: None,
        }
    }

    fn week(players: Vec<PlayerSummary>) -> Summary {
        Summary { start: date(2026, 9, 14), end: date(2026, 9, 20), players }
    }

    #[test]
    fn weekly_message_matches_spec_shape() {
        let msgs = format_weekly(&week(vec![summary_for("Faker#KR1")]), &london(), SPLIT_AT, 7, false);
        assert_eq!(msgs.len(), 1);
        let text = &msgs[0];
        assert!(text.starts_with("📊 <b>Week of 14–20 Sep</b>"));
        assert!(text.contains("<b>Faker#KR1</b>"));
        assert!(text.contains("23 games · 11h 42m · 63% WR"));
        assert!(text.contains("Ranked Solo") && text.contains("16 games · 8h 31m · 63% (10W 6L)"));
        let arena = text.lines().find(|l| l.contains("Arena")).unwrap();
        assert!(arena.contains("1 game ") && arena.contains("23m") && !arena.contains('%'));
        assert!(text.contains("Most played: Azir (7), Orianna (4)"));
        assert!(text.contains("(1 remake excluded from winrate)"));
    }

    #[test]
    fn weekly_player_with_no_games_and_no_players() {
        let idle = PlayerSummary { riot_id: "Idle#EUW".into(), ..Default::default() };
        assert!(format_weekly(&week(vec![idle]), &london(), SPLIT_AT, 7, false)[0].contains("no games"));
        assert!(format_weekly(&week(vec![]), &london(), SPLIT_AT, 7, false)[0].contains("No tracked players"));
    }

    #[test]
    fn weekly_splits_across_messages_under_the_telegram_limit() {
        let players = (0..40).map(|i| summary_for(&format!("Player{i:02}#EUW"))).collect();
        let messages = format_weekly(&week(players), &london(), SPLIT_AT, 7, false);
        assert!(messages.len() > 1);
        assert!(messages.iter().all(|m| chars(m) <= SPLIT_AT));
        let joined = messages.join("\n");
        assert!((0..40).all(|i| joined.contains(&format!("Player{i:02}#EUW"))));
        assert!(messages.iter().all(|m| m.matches("<pre>").count() == m.matches("</pre>").count()));
    }

    #[test]
    fn window_titles_say_what_the_window_is() {
        let s = week(vec![summary_for("Faker#KR1")]);
        assert!(format_weekly(&s, &london(), SPLIT_AT, 7, true)[0].starts_with("📊 <b>Week ending 20 Sep</b>"));
        assert!(format_weekly(&s, &london(), SPLIT_AT, 30, false)[0].starts_with("📊 <b>Last 30 days · 14–20 Sep</b>"));
        assert!(format_weekly(&s, &london(), SPLIT_AT, 1, false)[0].starts_with("📊 <b>Last 1 day"));
    }

    #[test]
    fn longest_session_line_uses_local_weekday_and_part_of_day() {
        let mut ps = summary_for("Faker#KR1");
        // 2026-09-20 19:00 UTC is a Sunday evening in London (BST).
        ps.longest_session = Some(crate::summary::LongestSession { games: 4, started_ms: 1_789_930_800_000 });
        let text = format_player_summary(&ps, &london());
        assert!(text.contains("Longest session: 4 games, Sun evening"), "{text}");
    }

    #[test]
    fn a_marathon_session_message_stays_valid_html_under_the_limit() {
        let games: Vec<Match> = (0..150).map(|i| make_match(&format!("m{i}"), T0 + i * 40 * MIN, 30)).collect();
        let text = format_session(&make_player_named("Alice", "EUW"), &games);
        assert!(chars(&text) <= SPLIT_AT);
        assert!(text.matches("<pre>").count() == 1 && text.matches("</pre>").count() == 1);
        assert!(text.contains("earlier games not shown") && text.contains("150 games"));
    }
}
