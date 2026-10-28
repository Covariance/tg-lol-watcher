//! I/O shell around `sessions.rs`: assigns ingested matches to sessions, runs the
//! flush check, and recovers open sessions after a restart.

use std::sync::{Arc, Mutex};

use crate::clock::Clock;
use crate::config::{Mode, NotificationConfig};
use crate::db::Database;
use crate::error::Result;
use crate::models::{Match, Player, Session};
use crate::notifier::SessionNotifier;
use crate::riot::Riot;
use crate::sessions::{assign, decide_flush, flush_delay_s};

pub trait FlushScheduler: Send + Sync {
    /// (Re)schedule the flush check for a session, replacing any pending one.
    fn schedule(&self, session_id: i64, delay_s: f64);
}

pub struct SessionService {
    db: Arc<Database>,
    riot: Arc<dyn Riot>,
    notifier: Arc<dyn SessionNotifier>,
    scheduler: Arc<dyn FlushScheduler>,
    cfg: NotificationConfig,
    clock: Clock,
    /// Serialises assignment vs. flush decisions.
    lock: Mutex<()>,
}

impl SessionService {
    pub fn new(
        db: Arc<Database>,
        riot: Arc<dyn Riot>,
        notifier: Arc<dyn SessionNotifier>,
        scheduler: Arc<dyn FlushScheduler>,
        cfg: NotificationConfig,
        clock: Clock,
    ) -> Self {
        Self { db, riot, notifier, scheduler, cfg, clock, lock: Mutex::new(()) }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    // -- ingestion ------------------------------------------------------------

    /// Called once for each *newly ingested* match, oldest first.
    pub fn on_match(&self, player: &Player, m: &Match) -> Result<()> {
        match self.cfg.mode {
            Mode::Off => {
                log::debug!("notify decision: mode=off match={} player={} -> skip", m.match_id, player.riot_id());
                return Ok(());
            }
            Mode::PerGame => {
                let ok = self.notifier.send_game(player, m)?;
                log::debug!(
                    "notify decision: mode=per_game match={} player={} sent={ok}",
                    m.match_id,
                    player.riot_id()
                );
                return Ok(());
            }
            Mode::Session => {}
        }

        let _held = self.guard();
        let open = self.db.open_sessions(Some(&player.puuid))?.into_iter().next();
        let a = assign(open.as_ref(), m.start_ms, m.end_ms, self.cfg.gap_ms());

        if let Some(open) = open.as_ref().filter(|_| a.flush_open) {
            log::debug!("session={} stale (new match {} starts after gap); flushing now", open.id, m.match_id);
            self.flush_now(open, "superseded by a later game")?;
        }

        let session_id = match open.as_ref().filter(|_| a.attach) {
            Some(open) => {
                self.db.update_session(open.id, a.started_ms, a.last_end_ms, Some(a.flush_due_ms), 0)?;
                log::debug!(
                    "session={} attach match={} last_end={} flush_due={}",
                    open.id,
                    m.match_id,
                    a.last_end_ms,
                    a.flush_due_ms
                );
                open.id
            }
            None => {
                let id = self.db.create_session(&player.puuid, a.started_ms, a.last_end_ms, a.flush_due_ms)?;
                log::debug!(
                    "session={id} created for match={} player={} flush_due={}",
                    m.match_id,
                    player.riot_id(),
                    a.flush_due_ms
                );
                id
            }
        };
        self.db.attach_match(session_id, &m.match_id, &player.puuid)?;
        self.scheduler.schedule(session_id, ((a.flush_due_ms - self.now()) as f64 / 1000.0).max(0.0));
        Ok(())
    }

    // -- flush job ------------------------------------------------------------

    /// Re-arm the flush check after the job itself crashed.
    pub fn retry_flush_soon(&self, session_id: i64) {
        self.scheduler.schedule(session_id, (self.cfg.recheck_ms() / 1000) as f64);
    }

    /// Job callback: is the session over? If the player is still in a game, postpone;
    /// otherwise send the summary.
    pub fn flush(&self, session_id: i64) -> Result<()> {
        let Some(session) = self.db.get_session(session_id)?.filter(Session::is_open) else {
            log::debug!("session={session_id} flush job: already flushed/missing, ignoring");
            return Ok(());
        };
        let Some(player) = self.db.get_player(&session.puuid)? else {
            log::warn!("session={session_id} belongs to unknown player; closing silently");
            return Ok(self.db.mark_flushed(session_id, self.now())?);
        };

        // Network call outside the lock so the poller isn't blocked behind it.
        let in_game = match self.riot.is_in_game(&player.puuid, &player.platform) {
            Ok(live) => Some(live),
            Err(e) => {
                log::info!("session={session_id} spectator lookup failed ({}: {e})", e.kind());
                None
            }
        };

        let _held = self.guard();
        // Re-read: a new match may have attached while we were waiting on Riot.
        let Some(session) = self.db.get_session(session_id)?.filter(Session::is_open) else {
            return Ok(());
        };
        let now = self.now();
        if let Some(due) = session.flush_due_ms.filter(|&due| due > now) {
            let remaining = (due - now) as f64 / 1000.0;
            log::debug!(
                "session={session_id} not due yet ({remaining:.0}s left: extended while checking \
                 spectator, or job fired early); rescheduling"
            );
            self.scheduler.schedule(session_id, remaining);
            return Ok(());
        }
        let decision = decide_flush(session.postpones, in_game, self.cfg.max_postpones, now, self.cfg.recheck_ms());
        log::debug!("session={session_id} flush decision: flush={} ({})", decision.flush, decision.reason);
        if !decision.flush {
            let due = decision.flush_due_ms.expect("a postpone carries a due time");
            self.db.set_postpone(session_id, decision.postpones, due)?;
            self.scheduler.schedule(session_id, (self.cfg.recheck_ms() / 1000) as f64);
            return Ok(());
        }
        self.flush_now(&session, &decision.reason)
    }

    /// Send the summary and mark the session flushed. Caller holds the lock.
    fn flush_now(&self, session: &Session, reason: &str) -> Result<()> {
        let player = self.db.get_player(&session.puuid)?;
        let matches = self.db.session_matches(session.id)?;
        let Some(player) = player.filter(|_| !matches.is_empty()) else {
            log::warn!("session={} has no player/matches; closing silently", session.id);
            return Ok(self.db.mark_flushed(session.id, self.now())?);
        };
        let sent = self.notifier.send_session(&player, &matches).unwrap_or_else(|e| {
            log::error!("session={} notification failed: {e}", session.id);
            false
        });
        if !sent && session.postpones < self.cfg.max_postpones {
            // Delivery failed: retry later rather than losing the notification.
            let postpones = session.postpones + 1;
            self.db.set_postpone(session.id, postpones, self.now() + self.cfg.recheck_ms())?;
            self.scheduler.schedule(session.id, (self.cfg.recheck_ms() / 1000) as f64);
            log::warn!(
                "session={} delivery failed; retry {postpones}/{} in {}m",
                session.id,
                self.cfg.max_postpones,
                self.cfg.flush_recheck_minutes
            );
            return Ok(());
        }
        self.db.mark_flushed(session.id, self.now())?;
        log::info!(
            "session={} flushed ({reason}) player={} games={} delivered={sent}",
            session.id,
            player.riot_id(),
            matches.len()
        );
        Ok(())
    }

    // -- restart recovery -----------------------------------------------------

    /// Reschedule every open session. Overdue ones run immediately.
    pub fn recover(&self) -> Result<usize> {
        let sessions = self.db.open_sessions(None)?;
        let now = self.now();
        for s in &sessions {
            let delay = flush_delay_s(s, now);
            log::info!("session={} recovered: due in {delay:.0}s", s.id);
            self.scheduler.schedule(s.id, delay);
        }
        Ok(sessions.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::riot::RiotError;
    use crate::testutil::{Env, FakeScheduler, MIN, T0, make_match};

    fn ids(matches: &[Match]) -> Vec<&str> {
        matches.iter().map(|m| m.match_id.as_str()).collect()
    }

    #[test]
    fn two_games_5_minutes_apart_one_session_one_notification() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        env.ingest(make_match("m2", T0 + 35 * MIN, 30)); // 5 min after m1 ended

        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1);
        let (sid, delay) = env.scheduler.last();
        assert_eq!(delay, ((65 + 15) * 60) as f64 - (env.clock.now() - T0) as f64 / 1000.0);

        env.clock.set(T0 + 80 * MIN);
        env.service.flush(sid).unwrap();
        let sent = env.notifier.sessions();
        assert_eq!(sent.len(), 1);
        assert_eq!(ids(&sent[0].1), ["m1", "m2"]);
    }

    #[test]
    fn two_games_40_minutes_apart_two_sessions_two_notifications() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        env.clock.set(T0 + 30 * MIN);
        env.ingest(make_match("m2", T0 + 70 * MIN, 30)); // 40 min after m1 ended

        // m1's session was closed the moment the later game showed it was over.
        assert_eq!(env.notifier.sessions().len(), 1);
        assert_eq!(ids(&env.notifier.sessions()[0].1), ["m1"]);

        let (sid, _) = env.scheduler.last();
        env.clock.set(T0 + 115 * MIN);
        env.service.flush(sid).unwrap();
        let all: Vec<Vec<String>> =
            env.notifier.sessions().iter().map(|(_, ms)| ms.iter().map(|m| m.match_id.clone()).collect()).collect();
        assert_eq!(all, [vec!["m1".to_owned()], vec!["m2".to_owned()]]);
    }

    #[test]
    fn in_game_postpones_and_next_game_joins_the_same_session() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        let (sid, _) = env.scheduler.last();

        env.clock.set(T0 + 45 * MIN); // flush due, but the player queued up again
        env.riot.set_in_game(Ok(true));
        env.service.flush(sid).unwrap();
        assert!(env.notifier.sessions().is_empty());
        assert_eq!(env.scheduler.last(), (sid, 300.0));
        let stored = env.db.get_session(sid).unwrap().unwrap();
        assert!(stored.postpones == 1 && stored.is_open());

        // The game they were in finishes; it started 10 min after m1 ended.
        env.clock.set(T0 + 75 * MIN);
        env.ingest(make_match("m2", T0 + 40 * MIN, 30));
        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1); // same session, not a new one
        assert_eq!(env.db.get_session(sid).unwrap().unwrap().postpones, 0); // activity resets it

        env.riot.set_in_game(Ok(false));
        env.clock.set(T0 + 85 * MIN);
        env.service.flush(sid).unwrap();
        assert_eq!(env.notifier.sessions().len(), 1);
        assert_eq!(ids(&env.notifier.sessions()[0].1), ["m1", "m2"]);
    }

    #[test]
    fn max_postpones_exceeded_flushes_anyway() {
        let env = Env::with_cfg(NotificationConfig { max_postpones: 2, ..Default::default() });
        env.ingest(make_match("m1", T0, 30));
        let (sid, _) = env.scheduler.last();
        env.riot.set_in_game(Ok(true)); // spectator "lies" forever

        for at in [45, 50] {
            env.clock.set(T0 + at * MIN);
            env.service.flush(sid).unwrap(); // postpones 1, 2
        }
        assert!(env.notifier.sessions().is_empty());
        env.clock.set(T0 + 55 * MIN);
        env.service.flush(sid).unwrap(); // 3 > max_postpones -> flush
        assert_eq!(env.notifier.sessions().len(), 1);
        assert_eq!(env.db.get_session(sid).unwrap().unwrap().status, "flushed");
    }

    #[test]
    fn out_of_order_arrival_yields_correct_last_end() {
        let env = Env::new();
        env.ingest(make_match("late", T0 + 40 * MIN, 30)); // ends T0+70
        env.ingest(make_match("early", T0, 30)); // arrives second, ended earlier
        let (sid, _) = env.scheduler.last();
        let s = env.db.get_session(sid).unwrap().unwrap();
        assert_eq!((s.started_ms, s.last_end_ms, s.flush_due_ms), (T0, T0 + 70 * MIN, Some(T0 + 85 * MIN)));
        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1);
    }

    #[test]
    fn restart_with_open_session_still_flushes_once() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        let (sid, _) = env.scheduler.last();

        // "Process dies": all in-memory jobs are gone. A fresh service + scheduler start
        // up on the same database while the session is already overdue.
        env.clock.set(T0 + 180 * MIN);
        let fresh = Arc::new(FakeScheduler::default());
        let restarted = env.make_service(fresh.clone());
        assert_eq!(restarted.recover().unwrap(), 1);
        assert_eq!(fresh.calls(), [(sid, 0.0)]); // overdue -> immediate check

        restarted.flush(sid).unwrap();
        restarted.flush(sid).unwrap(); // a duplicate job must be harmless
        assert_eq!(env.notifier.sessions().len(), 1);
    }

    #[test]
    fn recover_reschedules_future_due_time() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        env.clock.set(T0 + 35 * MIN); // 10 min until due (T0+45)
        let scheduler = Arc::new(FakeScheduler::default());
        env.make_service(scheduler.clone()).recover().unwrap();
        assert_eq!(scheduler.calls(), [(1, 600.0)]);
    }

    #[test]
    fn single_game_is_exactly_one_notification() {
        let env = Env::new();
        env.ingest(make_match("solo", T0, 25));
        let (sid, _) = env.scheduler.last();
        env.clock.set(T0 + 40 * MIN);
        env.service.flush(sid).unwrap();
        assert_eq!(env.notifier.sessions().len(), 1);
        assert_eq!(env.notifier.sessions()[0].1.len(), 1);
    }

    #[test]
    fn spectator_outage_does_not_delay_the_flush() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        let (sid, _) = env.scheduler.last();
        env.riot.set_in_game(Err(RiotError::Unavailable("spectator down".into())));
        env.clock.set(T0 + 45 * MIN);
        env.service.flush(sid).unwrap();
        assert_eq!(env.notifier.sessions().len(), 1);
    }

    #[test]
    fn flush_job_firing_early_reschedules_instead_of_dropping() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        let (sid, _) = env.scheduler.last();
        env.clock.set(T0 + 44 * MIN); // one minute before due
        env.service.flush(sid).unwrap();
        assert!(env.notifier.sessions().is_empty());
        assert_eq!(env.scheduler.last(), (sid, 60.0));
    }

    #[test]
    fn delivery_failure_retries_instead_of_losing_the_notification() {
        let env = Env::new();
        env.ingest(make_match("m1", T0, 30));
        let (sid, _) = env.scheduler.last();
        env.clock.set(T0 + 45 * MIN);
        env.notifier.set_ok(false);
        env.service.flush(sid).unwrap();
        assert!(env.db.get_session(sid).unwrap().unwrap().is_open());
        assert_eq!(env.scheduler.last(), (sid, 300.0));

        env.notifier.set_ok(true);
        env.clock.set(T0 + 50 * MIN);
        env.service.flush(sid).unwrap();
        assert_eq!(env.notifier.sessions().len(), 1);
    }

    #[test]
    fn per_game_mode_sends_each_game_and_creates_no_sessions() {
        let env = Env::with_cfg(NotificationConfig { mode: Mode::PerGame, ..Default::default() });
        env.ingest(make_match("m1", T0, 30));
        env.ingest(make_match("m2", T0 + 35 * MIN, 30));
        let games: Vec<String> = env.notifier.games().into_iter().map(|(_, m)| m.match_id).collect();
        assert_eq!(games, ["m1", "m2"]);
        assert!(env.db.open_sessions(None).unwrap().is_empty());
    }

    #[test]
    fn off_mode_sends_nothing() {
        let env = Env::with_cfg(NotificationConfig { mode: Mode::Off, ..Default::default() });
        env.ingest(make_match("m1", T0, 30));
        assert!(env.notifier.games().is_empty() && env.notifier.sessions().is_empty());
        assert!(env.scheduler.calls().is_empty());
    }
}
