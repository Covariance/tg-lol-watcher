//! Session coalescing — PURE LOGIC.
//!
//! No clock, no I/O: every function takes explicit times, so the subtle cases
//! (out-of-order arrival, postpones, restarts) are unit-testable with a frozen clock.
//! `flusher.rs` is the I/O shell around these decisions.

use crate::models::{Match, Session};

/// What to do with a newly ingested match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// true: join the open session. false: start a new one.
    pub attach: bool,
    /// true: the open session is stale and must flush right now.
    pub flush_open: bool,
    pub started_ms: i64,
    pub last_end_ms: i64,
    pub flush_due_ms: i64,
}

/// Decide where a match belongs.
///
/// A match joins the open session when its interval is within `gap_ms` of the
/// session's [started, last_end] span. Out-of-order arrival is handled with min/max,
/// and `flush_due_ms` is always re-derived from the true latest end.
pub fn assign(open: Option<&Session>, start_ms: i64, end_ms: i64, gap_ms: i64) -> Assignment {
    let fresh = |flush_open| Assignment {
        attach: false,
        flush_open,
        started_ms: start_ms,
        last_end_ms: end_ms,
        flush_due_ms: end_ms + gap_ms,
    };
    let Some(open) = open else {
        return fresh(false);
    };
    let starts_before_gap_closes = start_ms <= open.last_end_ms + gap_ms;
    let ends_after_gap_opens = end_ms >= open.started_ms - gap_ms;
    if starts_before_gap_closes && ends_after_gap_opens {
        let last_end = open.last_end_ms.max(end_ms);
        return Assignment {
            attach: true,
            flush_open: false,
            started_ms: open.started_ms.min(start_ms),
            last_end_ms: last_end,
            flush_due_ms: last_end + gap_ms,
        };
    }
    // Entirely older than the open session (late arrival): it's its own historical
    // session and the open one is untouched. Otherwise the open session is stale
    // (e.g. we were down) and is flushed before the new one.
    let older = end_ms + gap_ms < open.started_ms;
    fresh(!older)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushDecision {
    pub flush: bool,
    pub postpones: i64,
    /// New due time when postponing.
    pub flush_due_ms: Option<i64>,
    /// For the DEBUG decision log.
    pub reason: String,
}

/// The flush-job decision.
///
/// `in_game = None` means the spectator lookup failed. That is deliberately treated
/// as "not in game": a spectator outage must delay nothing.
pub fn decide_flush(
    postpones: i64,
    in_game: Option<bool>,
    max_postpones: i64,
    now_ms: i64,
    recheck_ms: i64,
) -> FlushDecision {
    match in_game {
        None => FlushDecision {
            flush: true,
            postpones,
            flush_due_ms: None,
            reason: "spectator unavailable, treating as not in game".into(),
        },
        Some(false) => FlushDecision { flush: true, postpones, flush_due_ms: None, reason: "not in game".into() },
        Some(true) => {
            let postpones = postpones + 1;
            if postpones > max_postpones {
                FlushDecision {
                    flush: true,
                    postpones,
                    flush_due_ms: None,
                    reason: format!("still in game but postpones {postpones} > max {max_postpones}"),
                }
            } else {
                FlushDecision {
                    flush: false,
                    postpones,
                    flush_due_ms: Some(now_ms + recheck_ms),
                    reason: format!("in game, postponing ({postpones}/{max_postpones})"),
                }
            }
        }
    }
}

/// Seconds until the session's flush check should run. 0 if overdue — this is what
/// restart recovery uses so a restart can't swallow a notification.
pub fn flush_delay_s(session: &Session, now_ms: i64) -> f64 {
    let due = session.flush_due_ms.unwrap_or(now_ms);
    ((due - now_ms) as f64 / 1000.0).max(0.0)
}

/// Split matches into sessions by the same gap rule (used by summaries).
pub fn group_matches(matches: &[Match], gap_ms: i64) -> Vec<Vec<&Match>> {
    let mut ordered: Vec<&Match> = matches.iter().collect();
    ordered.sort_by_key(|m| m.start_ms);
    let mut groups: Vec<Vec<&Match>> = Vec::new();
    let mut latest_end = 0;
    for m in ordered {
        match groups.last_mut() {
            Some(group) if m.start_ms <= latest_end + gap_ms => {
                group.push(m);
                latest_end = latest_end.max(m.end_ms);
            }
            _ => {
                groups.push(vec![m]);
                latest_end = m.end_ms;
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MIN, T0, make_match};

    const GAP: i64 = 15 * MIN;

    fn session(start: i64, end: i64) -> Session {
        Session {
            id: 1,
            puuid: "p".into(),
            started_ms: start,
            last_end_ms: end,
            status: "open".into(),
            flush_due_ms: Some(end + GAP),
            postpones: 0,
            notified_at: None,
        }
    }

    fn default_session() -> Session {
        session(T0, T0 + 30 * MIN)
    }

    #[test]
    fn first_match_creates_session_due_after_gap() {
        let a = assign(None, T0, T0 + 30 * MIN, GAP);
        assert_eq!((a.attach, a.flush_open), (false, false));
        assert_eq!((a.started_ms, a.last_end_ms, a.flush_due_ms), (T0, T0 + 30 * MIN, T0 + 45 * MIN));
    }

    #[test]
    fn within_gap_attaches_and_extends() {
        let a = assign(Some(&default_session()), T0 + 35 * MIN, T0 + 65 * MIN, GAP);
        assert!(a.attach && !a.flush_open);
        assert_eq!(a.last_end_ms, T0 + 65 * MIN);
        assert_eq!(a.flush_due_ms, T0 + 80 * MIN);
    }

    #[test]
    fn gap_boundary_is_inclusive() {
        let s = default_session();
        assert!(assign(Some(&s), T0 + 30 * MIN + GAP, T0 + 90 * MIN, GAP).attach);
        assert!(!assign(Some(&s), T0 + 30 * MIN + GAP + 1, T0 + 90 * MIN, GAP).attach);
    }

    #[test]
    fn after_gap_flushes_stale_session_and_starts_new_one() {
        let a = assign(Some(&default_session()), T0 + 70 * MIN, T0 + 100 * MIN, GAP);
        assert_eq!((a.attach, a.flush_open), (false, true));
        assert_eq!(a.started_ms, T0 + 70 * MIN);
    }

    #[test]
    fn out_of_order_keeps_true_latest_end() {
        // A game that ended *before* the session's current last_end arrives late.
        let s = session(T0 + 40 * MIN, T0 + 70 * MIN);
        let a = assign(Some(&s), T0 + 20 * MIN, T0 + 38 * MIN, GAP);
        assert!(a.attach);
        assert_eq!(a.started_ms, T0 + 20 * MIN);
        assert_eq!(a.last_end_ms, T0 + 70 * MIN); // max(), not overwritten by the older game
        assert_eq!(a.flush_due_ms, T0 + 70 * MIN + GAP); // re-derived from the true latest end
    }

    #[test]
    fn much_older_match_does_not_flush_or_join_open_session() {
        let s = session(T0 + 300 * MIN, T0 + 330 * MIN);
        let a = assign(Some(&s), T0, T0 + 30 * MIN, GAP);
        assert_eq!((a.attach, a.flush_open), (false, false));
    }

    #[test]
    fn decide_flush_rules() {
        assert!(decide_flush(0, Some(false), 18, T0, 5 * MIN).flush);
        let d = decide_flush(0, None, 18, T0, 5 * MIN);
        assert!(d.flush && d.reason.contains("unavailable"));
        let d = decide_flush(0, Some(true), 2, T0, 5 * MIN);
        assert_eq!((d.flush, d.postpones, d.flush_due_ms), (false, 1, Some(T0 + 5 * MIN)));
        let d = decide_flush(2, Some(true), 2, T0, 5 * MIN);
        assert!(d.flush && d.postpones == 3);
    }

    #[test]
    fn flush_delay_overdue_is_zero() {
        let mut s = default_session();
        s.flush_due_ms = Some(T0 - 5 * MIN);
        assert_eq!(flush_delay_s(&s, T0), 0.0);
        s.flush_due_ms = Some(T0 + 90_000);
        assert_eq!(flush_delay_s(&s, T0), 90.0);
    }

    #[test]
    fn group_matches_splits_on_gap() {
        let ms = [make_match("a", T0, 30), make_match("b", T0 + 35 * MIN, 30), make_match("c", T0 + 200 * MIN, 30)];
        let ids: Vec<Vec<&str>> =
            group_matches(&ms, GAP).iter().map(|g| g.iter().map(|m| m.match_id.as_str()).collect()).collect();
        assert_eq!(ids, [vec!["a", "b"], vec!["c"]]);
    }
}
