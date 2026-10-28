//! Weekly / windowed aggregation. Pure: matches in, structs out.

use std::collections::{BTreeMap, HashMap};

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{ToSpan, Zoned};

use crate::models::{Match, group_sort_key, queue_group};
use crate::sessions::group_matches;

/// Whole-number percentage, rounding half up (10W 6L is 63%, not banker's 62%).
fn percent(part: i64, whole: i64) -> i64 {
    (200 * part + whole) / (2 * whole)
}

fn winrate(wins: i64, losses: i64) -> Option<i64> {
    let decided = wins + losses;
    (decided > 0).then(|| percent(wins, decided))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStats {
    pub group: String,
    pub games: i64,
    pub seconds: i64,
    pub wins: i64,
    pub losses: i64,
}

impl QueueStats {
    pub fn winrate(&self) -> Option<i64> {
        winrate(self.wins, self.losses)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongestSession {
    pub games: i64,
    pub started_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerSummary {
    pub riot_id: String,
    pub games: i64,
    pub seconds: i64,
    pub wins: i64,
    pub losses: i64,
    pub remakes: i64,
    pub groups: Vec<QueueStats>,
    pub top_champions: Vec<(String, i64)>,
    pub longest_session: Option<LongestSession>,
}

impl PlayerSummary {
    pub fn winrate(&self) -> Option<i64> {
        winrate(self.wins, self.losses)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub start: Date,
    pub end: Date,
    pub players: Vec<PlayerSummary>,
}

/// [start, end) for the last `days` days ending `now`, in now's time zone.
///
/// Default: starts at local midnight, so `days = 7` on a Sunday evening is exactly
/// Monday 00:00 .. now (what `/week` shows).
///
/// `rolling`: starts exactly `days` calendar days before now (same local clock time).
/// The scheduled weekly summary uses it so consecutive runs tile the timeline with no
/// gap: the midnight-aligned window would drop Sunday 20:00-24:00 from every week.
pub fn window_days(now: &Zoned, days: i64, rolling: bool) -> (Zoned, Zoned) {
    if rolling {
        return (now.checked_sub(days.days()).expect("window in range"), now.clone());
    }
    let first_day = now.date().checked_sub((days - 1).days()).expect("window in range");
    let start = first_day.to_zoned(now.time_zone().clone()).expect("local midnight in range");
    (start, now.clone())
}

pub fn summarize_player(riot_id: &str, matches: &[Match], gap_ms: i64) -> PlayerSummary {
    if matches.is_empty() {
        return PlayerSummary { riot_id: riot_id.into(), ..Default::default() };
    }
    let count = |pred: fn(&Match) -> bool, ms: &[&Match]| ms.iter().filter(|m| pred(m)).count() as i64;
    let all: Vec<&Match> = matches.iter().collect();

    let mut by_group: HashMap<String, Vec<&Match>> = HashMap::new();
    for m in matches {
        by_group.entry(queue_group(m.queue_id)).or_default().push(m);
    }
    let mut groups: Vec<QueueStats> = by_group
        .into_iter()
        .map(|(group, ms)| QueueStats {
            games: ms.len() as i64,
            seconds: ms.iter().map(|m| m.duration_s).sum(),
            wins: count(Match::is_win, &ms),
            losses: count(Match::is_loss, &ms),
            group,
        })
        .collect();
    groups.sort_by(|a, b| group_sort_key(&a.group).cmp(&group_sort_key(&b.group)));

    let mut champs: BTreeMap<&str, i64> = BTreeMap::new();
    for m in matches {
        *champs.entry(&m.champion).or_default() += 1;
    }
    // Highest count first; ties broken alphabetically so output is deterministic.
    let mut top: Vec<(String, i64)> = champs.into_iter().map(|(c, n)| (c.to_owned(), n)).collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(3);

    let sessions = group_matches(matches, gap_ms);
    // max() keeps the last of equal maxima; the first is wanted (Python's max).
    let longest = sessions.iter().rev().max_by_key(|g| g.len()).expect("non-empty");
    let longest_session =
        (longest.len() >= 2).then(|| LongestSession { games: longest.len() as i64, started_ms: longest[0].start_ms });

    PlayerSummary {
        riot_id: riot_id.into(),
        games: matches.len() as i64,
        seconds: matches.iter().map(|m| m.duration_s).sum(),
        wins: count(Match::is_win, &all),
        losses: count(Match::is_loss, &all),
        remakes: matches.iter().filter(|m| m.remake).count() as i64,
        groups,
        top_champions: top,
        longest_session,
    }
}

pub fn build_summary(
    start: &Zoned,
    end: &Zoned,
    tz: &TimeZone,
    players: &[(String, Vec<Match>)],
    gap_ms: i64,
) -> Summary {
    Summary {
        start: start.with_time_zone(tz.clone()).date(),
        end: end.with_time_zone(tz.clone()).date(),
        players: players.iter().map(|(rid, ms)| summarize_player(rid, ms, gap_ms)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MIN, T0, london, make_match, make_match_with, zoned};

    const GAP: i64 = 15 * MIN;

    #[test]
    fn no_matches_is_an_empty_summary() {
        let s = summarize_player("A#1", &[], GAP);
        assert!(s.games == 0 && s.winrate().is_none() && s.groups.is_empty());
    }

    #[test]
    fn winrate_excludes_remakes_and_unknown_results() {
        let ms = vec![
            make_match_with("1", T0, 30, |m| m.win = 1),
            make_match_with("2", T0 + 60 * MIN, 30, |m| m.win = 0),
            make_match_with("3", T0 + 120 * MIN, 30, |m| m.win = 1),
            make_match_with("4", T0 + 180 * MIN, 3, |m| {
                m.win = 0;
                m.remake = true;
            }),
            make_match_with("5", T0 + 240 * MIN, 25, |m| {
                m.queue_id = 1700;
                m.win = -1;
            }),
        ];
        let s = summarize_player("A#1", &ms, GAP);
        assert_eq!(s.games, 5); // remakes and arena still count as games...
        assert_eq!((s.wins, s.losses, s.remakes), (2, 1, 1));
        assert_eq!(s.winrate(), Some(67)); // ...but only decided games enter the winrate
        assert_eq!(s.seconds, (30 * 3 + 3 + 25) * 60);
    }

    #[test]
    fn winrate_rounds_half_up() {
        let ms: Vec<Match> = (0..16)
            .map(|i| make_match_with(&i.to_string(), T0 + i * 60 * MIN, 30, |m| m.win = (i < 10) as i64))
            .collect();
        assert_eq!(summarize_player("A#1", &ms, GAP).winrate(), Some(63)); // 62.5 -> 63
    }

    #[test]
    fn groups_are_split_by_queue_and_ordered() {
        let ms = vec![
            make_match_with("a", T0, 30, |m| {
                m.queue_id = 450;
                m.champion = "Ziggs".into();
            }),
            make_match_with("b", T0 + 60 * MIN, 30, |m| m.queue_id = 420),
            make_match_with("c", T0 + 120 * MIN, 30, |m| m.queue_id = 420),
            make_match_with("d", T0 + 180 * MIN, 30, |m| {
                m.queue_id = 1700;
                m.win = -1;
            }),
            make_match_with("e", T0 + 240 * MIN, 30, |m| m.queue_id = 9999),
        ];
        let groups = summarize_player("A#1", &ms, GAP).groups;
        let got: Vec<(&str, i64)> = groups.iter().map(|g| (g.group.as_str(), g.games)).collect();
        assert_eq!(got, [("Ranked Solo", 2), ("ARAM", 1), ("Arena", 1), ("Queue 9999", 1)]);
        assert_eq!(groups[2].winrate(), None); // Arena: no decided games -> no winrate
    }

    #[test]
    fn most_played_champions_are_deterministic() {
        let champs = ["Azir", "Ahri", "Azir", "Zed", "Ahri", "Azir", "Lux"];
        let ms: Vec<Match> = champs
            .iter()
            .enumerate()
            .map(|(i, c)| make_match_with(&i.to_string(), T0 + i as i64 * 60 * MIN, 30, |m| m.champion = (*c).into()))
            .collect();
        let top = summarize_player("A#1", &ms, GAP).top_champions;
        assert_eq!(top, [("Azir".into(), 3), ("Ahri".into(), 2), ("Lux".into(), 1)]);
    }

    #[test]
    fn longest_session_uses_the_gap_rule() {
        let ms = vec![
            make_match("a", T0, 30),
            make_match("b", T0 + 35 * MIN, 30),
            make_match("c", T0 + 70 * MIN, 30),
            make_match("d", T0 + 500 * MIN, 30),
        ];
        let longest = summarize_player("A#1", &ms, GAP).longest_session.unwrap();
        assert_eq!((longest.games, longest.started_ms), (3, T0));
        // A lone game isn't a "session" worth reporting.
        assert!(summarize_player("A#1", &[make_match("a", T0, 30)], GAP).longest_session.is_none());
    }

    #[test]
    fn window_days_is_seven_local_days_from_monday_midnight() {
        let now = zoned(2026, 9, 20, 20, 0); // Sunday 20:00
        let (start, end) = window_days(&now, 7, false);
        assert_eq!(start, zoned(2026, 9, 14, 0, 0)); // Monday 00:00
        assert_eq!(end, now);
    }

    #[test]
    fn window_start_is_local_midnight_across_a_dst_change() {
        let now = zoned(2026, 3, 30, 12, 0); // UK clocks went forward 29 Mar
        let (start, _) = window_days(&now, 3, false);
        assert_eq!((start.year(), start.month(), start.day(), start.hour()), (2026, 3, 28, 0));
        assert_ne!(start.offset(), now.offset()); // GMT vs BST: still midnight local
    }

    #[test]
    fn build_summary_dates_are_local() {
        let s = build_summary(
            &zoned(2026, 9, 14, 0, 0),
            &zoned(2026, 9, 20, 20, 0),
            &london(),
            &[("A#1".into(), vec![make_match("a", T0, 30)])],
            GAP,
        );
        assert_eq!((s.start.day(), s.end.day(), s.players[0].games), (14, 20, 1));
    }

    #[test]
    fn rolling_windows_tile_the_timeline_without_gaps() {
        // The scheduled weekly summary must not lose Sunday 20:00-24:00.
        let run1 = zoned(2026, 9, 20, 20, 0);
        let run2 = zoned(2026, 9, 27, 20, 0);
        let (_, end1) = window_days(&run1, 7, true);
        let (start2, _) = window_days(&run2, 7, true);
        assert!(end1 == start2 && start2 == run1);
        assert!(window_days(&run2, 7, false).0 > end1); // the midnight window leaves a hole
    }

    #[test]
    fn rolling_window_keeps_the_local_clock_time_across_a_dst_change() {
        let run = zoned(2026, 3, 30, 20, 0); // BST; a week earlier was GMT
        let (start, _) = window_days(&run, 7, true);
        assert_eq!((start.day(), start.hour(), start.minute()), (23, 20, 0));
    }
}
