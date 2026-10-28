//! The job queue: repeating polls, the weekly summary, and one flush check per open
//! session. One thread, one heap, no runtime.
//!
//! Due times are wall-clock epoch ms and the thread never sleeps longer than a
//! minute at a time, so a suspended machine or a clock step can't strand a job.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};

use crate::clock::Clock;
use crate::flusher::FlushScheduler;

/// Longest single wait: re-reads the clock at least this often.
const MAX_NAP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Job {
    Poll,
    Names,
    Weekly,
    Flush(i64),
}

#[derive(Default)]
struct State {
    heap: BinaryHeap<Reverse<(i64, u64, Job)>>,
    /// Session id -> sequence number of its live flush entry; older entries are stale.
    flush_seq: HashMap<i64, u64>,
    seq: u64,
    stopped: bool,
}

pub struct Scheduler {
    state: Mutex<State>,
    wake: Condvar,
    clock: Clock,
}

impl Scheduler {
    pub fn new(clock: Clock) -> Self {
        Self { state: Mutex::default(), wake: Condvar::new(), clock }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    pub fn at(&self, due_ms: i64, job: Job) {
        let mut state = self.lock();
        state.seq += 1;
        let seq = state.seq;
        if let Job::Flush(id) = job {
            state.flush_seq.insert(id, seq);
        }
        state.heap.push(Reverse((due_ms, seq, job)));
        drop(state);
        self.wake.notify_one();
    }

    pub fn after(&self, delay_ms: i64, job: Job) {
        self.at(self.now() + delay_ms.max(0), job);
    }

    pub fn stop(&self) {
        self.lock().stopped = true;
        self.wake.notify_all();
    }

    /// The next job that is due at `now`, skipping superseded flush entries.
    pub fn pop_due(&self, now: i64) -> Option<(i64, Job)> {
        let mut state = self.lock();
        Self::pop_due_locked(&mut state, now)
    }

    fn pop_due_locked(state: &mut State, now: i64) -> Option<(i64, Job)> {
        while let Some(&Reverse((due, seq, job))) = state.heap.peek() {
            if due > now {
                return None;
            }
            state.heap.pop();
            if let Job::Flush(id) = job {
                if state.flush_seq.get(&id) != Some(&seq) {
                    continue; // rescheduled since: this entry is stale
                }
                state.flush_seq.remove(&id);
            }
            return Some((due, job));
        }
        None
    }

    /// Block until a job is due (or `stop`), and return it with its due time.
    pub fn next(&self) -> Option<(i64, Job)> {
        let mut state = self.lock();
        loop {
            if state.stopped {
                return None;
            }
            let now = self.now();
            if let Some(job) = Self::pop_due_locked(&mut state, now) {
                return Some(job);
            }
            let nap = match state.heap.peek() {
                Some(Reverse((due, _, _))) => Duration::from_millis((due - now).max(1) as u64).min(MAX_NAP),
                None => MAX_NAP,
            };
            state = self.wake.wait_timeout(state, nap).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    pub fn pending(&self) -> Vec<(i64, Job)> {
        let state = self.lock();
        let mut jobs: Vec<(i64, Job)> = state
            .heap
            .iter()
            .filter(|Reverse((_, seq, job))| match job {
                Job::Flush(id) => state.flush_seq.get(id) == Some(seq),
                _ => true,
            })
            .map(|Reverse((due, _, job))| (*due, *job))
            .collect();
        jobs.sort();
        jobs
    }
}

impl FlushScheduler for Scheduler {
    fn schedule(&self, session_id: i64, delay_s: f64) {
        self.after((delay_s * 1000.0).round() as i64, Job::Flush(session_id));
    }
}

/// The next repetition of a fixed-rate job: skips runs missed while busy or asleep.
pub fn next_repeat(previous_due: i64, interval_ms: i64, now: i64) -> i64 {
    let mut next = previous_due + interval_ms;
    if next <= now {
        next += ((now - next) / interval_ms + 1) * interval_ms;
    }
    next
}

/// The first `weekday hh:mm` (local, DST-aware) strictly after `now`.
pub fn next_weekly(now: Timestamp, tz: &TimeZone, weekday: Weekday, hour: i8, minute: i8) -> Zoned {
    let local = now.to_zoned(tz.clone());
    for days in 0..=7 {
        let day = local.date().checked_add(days.days()).expect("date in range");
        if day.weekday() != weekday {
            continue;
        }
        // A time that doesn't exist that day (DST gap) resolves to just after the gap.
        let at = day.at(hour, minute, 0, 0).to_zoned(tz.clone()).expect("time in range");
        if at.timestamp() > now {
            return at;
        }
    }
    unreachable!("a weekday recurs within eight days")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{T0, TestClock, london, zoned};

    #[test]
    fn jobs_come_out_in_due_order_and_a_reschedule_replaces_the_old_flush() {
        let clock = TestClock::new(T0);
        let s = Scheduler::new(clock.as_clock());
        s.at(T0 + 300, Job::Poll);
        s.schedule(7, 0.1); // T0+100
        s.schedule(7, 0.2); // replaces it: T0+200
        s.at(T0 + 50, Job::Names);

        assert_eq!(s.pop_due(T0 + 1_000), Some((T0 + 50, Job::Names)));
        assert_eq!(s.pop_due(T0 + 1_000), Some((T0 + 200, Job::Flush(7))));
        assert_eq!(s.pop_due(T0 + 1_000), Some((T0 + 300, Job::Poll)));
        assert_eq!(s.pop_due(T0 + 1_000), None);
    }

    #[test]
    fn nothing_runs_early() {
        let s = Scheduler::new(TestClock::new(T0).as_clock());
        s.at(T0 + 10, Job::Weekly);
        assert_eq!(s.pop_due(T0 + 9), None);
        assert_eq!(s.pending(), [(T0 + 10, Job::Weekly)]);
        assert_eq!(s.pop_due(T0 + 10), Some((T0 + 10, Job::Weekly)));
    }

    #[test]
    fn next_returns_due_jobs_and_stop_ends_it() {
        let s = Scheduler::new(TestClock::new(T0).as_clock());
        s.at(T0 - 1, Job::Poll);
        assert_eq!(s.next(), Some((T0 - 1, Job::Poll)));
        s.stop();
        assert_eq!(s.next(), None);
    }

    #[test]
    fn repeats_are_fixed_rate_and_skip_missed_runs() {
        assert_eq!(next_repeat(1_000, 120, 1_050), 1_120);
        assert_eq!(next_repeat(1_000, 120, 1_500), 1_600); // overran: next slot after now
        assert_eq!(next_repeat(1_000, 120, 1_120), 1_240);
    }

    #[test]
    fn weekly_fires_on_the_configured_local_day_and_time() {
        let tz = london();
        // Wednesday 16 Sep 2026 -> Sunday 20 Sep 20:00 BST.
        let next = next_weekly(zoned(2026, 9, 16, 12, 0).timestamp(), &tz, Weekday::Sunday, 20, 0);
        assert_eq!(next, zoned(2026, 9, 20, 20, 0));
        // Exactly at the time: the *next* week.
        let next = next_weekly(zoned(2026, 9, 20, 20, 0).timestamp(), &tz, Weekday::Sunday, 20, 0);
        assert_eq!(next, zoned(2026, 9, 27, 20, 0));
        // Same day, later: today.
        let next = next_weekly(zoned(2026, 9, 20, 9, 0).timestamp(), &tz, Weekday::Sunday, 20, 0);
        assert_eq!(next, zoned(2026, 9, 20, 20, 0));
    }

    #[test]
    fn weekly_keeps_local_time_across_dst() {
        let tz = london();
        // Clocks go back on Sunday 25 Oct 2026: 20:00 is GMT that day, BST the week before.
        let before = next_weekly(zoned(2026, 10, 17, 0, 0).timestamp(), &tz, Weekday::Sunday, 20, 0);
        let after = next_weekly(before.timestamp(), &tz, Weekday::Sunday, 20, 0);
        assert_eq!((before.hour(), after.hour(), after.day()), (20, 20, 25));
        assert_ne!(before.offset(), after.offset());
    }
}
