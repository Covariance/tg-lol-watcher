//! Periodic match-history scan and idempotent ingestion.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::clock::Clock;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::flusher::SessionService;
use crate::models::{Match, MatchData, Player};
use crate::notifier::Alerter;
use crate::riot::parsing::MAX_GAME_S;
use crate::riot::{Riot, RiotError};

pub const DAY_MS: i64 = 86_400_000;
/// Re-scan this far behind last_seen. Riot's startTime semantics (start vs end of
/// game) and match-history lag both make an exact cut-off fragile; ingestion is
/// idempotent and known ids cost no API call, so overlapping is free.
pub const OVERLAP_MS: i64 = MAX_GAME_S * 1000;
const INITIAL_BACKFILL_DAYS: i64 = 7;
const NAME_REFRESH_MS: i64 = 7 * DAY_MS;
/// A match Riot lists but can't serve yet (404) is retried soon: the details lag the
/// list. One that is unparseable or doesn't include the player is deterministic.
const NOT_FOUND_RETRY_MS: i64 = 30 * 60_000;
const UNUSABLE_RETRY_MS: i64 = DAY_MS;
const BAD_MATCHES_CAP: usize = 1000;

pub const AUTH_ALERT: &str = "Riot API rejected the key (403). The key is probably expired — replace \
                              RIOT_API_KEY and restart. Polling is paused until it works.";

/// What `ingest` should do; the defaults are a normal poll.
#[derive(Debug, Clone, Copy, Default)]
pub struct IngestOptions {
    pub since_ms: Option<i64>,
    /// None: notify unless this is the player's first sight.
    pub notify: Option<bool>,
    pub count: Option<u32>,
}

pub struct Poller {
    db: Arc<Database>,
    riot: Arc<dyn Riot>,
    sessions: Option<Arc<SessionService>>,
    alerter: Option<Arc<Alerter>>,
    clock: Clock,
    /// (match_id, puuid) we can't use right now -> when to try again (epoch ms).
    bad_matches: Mutex<HashMap<(String, String), i64>>,
}

impl Poller {
    pub fn new(
        db: Arc<Database>,
        riot: Arc<dyn Riot>,
        sessions: Option<Arc<SessionService>>,
        alerter: Option<Arc<Alerter>>,
        clock: Clock,
    ) -> Self {
        Self { db, riot, sessions, alerter, clock, bad_matches: Mutex::new(HashMap::new()) }
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn alert(&self, key: &str, text: &str) {
        if let Some(alerter) = &self.alerter {
            alerter.alert(key, text);
        }
    }

    // -- the scheduled job ----------------------------------------------------

    /// Scan every active player. Never fails: one bad player or a Riot hiccup must not
    /// kill the loop. Returns the number of new matches.
    pub fn poll_once(&self) -> usize {
        let players = match self.db.list_players(true) {
            Ok(players) => players,
            Err(e) => {
                log::error!("poll: could not list players: {e}");
                return 0;
            }
        };
        let mut cache = HashMap::new();
        let mut total = 0;
        for player in &players {
            match self.ingest(player, &mut cache, IngestOptions::default()) {
                Ok(n) => total += n,
                Err(Error::Riot(RiotError::Auth(e))) => {
                    log::error!("poll: Riot auth failure: {e}");
                    self.alert("riot_auth", AUTH_ALERT);
                    break; // every remaining call would fail too
                }
                Err(Error::Riot(e @ RiotError::RateLimited { .. })) => {
                    log::warn!("poll: rate limited ({e}); ending this cycle early");
                    break;
                }
                Err(Error::Riot(RiotError::BadRequest(e))) => log::error!(
                    "poll: Riot rejected the request for {} ({e}). If the API key was recently changed, \
                     PUUIDs are stale: run `lolwatch repuuid`",
                    player.riot_id()
                ),
                Err(Error::Riot(e)) if !matches!(e, RiotError::Unexpected(_)) => {
                    log::warn!("poll: {} for {}: {e}", e.kind(), player.riot_id());
                }
                Err(e) => {
                    log::error!("poll: unexpected failure for {}: {}: {e}", player.riot_id(), e.kind());
                    self.alert(
                        "poll_error",
                        &format!("Poller error for {}: {}: {e}. Still running.", player.riot_id(), e.kind()),
                    );
                }
            }
        }
        if total > 0 {
            log::info!("poll: ingested {total} new match(es) for {} player(s)", players.len());
        }
        total
    }

    // -- ingestion ------------------------------------------------------------

    /// Fetch and store matches we haven't seen. Returns how many were new.
    ///
    /// A player with no `last_seen_ms` is being seen for the first time: their recent
    /// history is stored *silently* (no session/notification for games that ended
    /// before we started watching).
    pub fn ingest(
        &self,
        player: &Player,
        cache: &mut HashMap<String, MatchData>,
        opts: IngestOptions,
    ) -> Result<usize> {
        let first_time = player.last_seen_ms.is_none();
        let started_at = self.now();
        let notify = opts.notify.unwrap_or(!first_time);
        let count = opts.count.unwrap_or(20);
        let since_ms = opts.since_ms.unwrap_or_else(|| match player.last_seen_ms {
            None => started_at - INITIAL_BACKFILL_DAYS * DAY_MS,
            Some(seen) => seen - OVERLAP_MS,
        });

        let ids = self.riot.recent_match_ids(&player.puuid, &player.platform, Some(since_ms), count)?;
        if ids.len() >= count as usize {
            log::warn!(
                "ingest: {} returned {} ids (the page limit); older games in the window may be missing",
                player.riot_id(),
                ids.len()
            );
        }
        let known = self.db.known_match_ids(&player.puuid, &ids)?;
        let fresh: Vec<&String> =
            ids.iter().filter(|id| !known.contains(*id) && !self.is_bad(id, &player.puuid)).collect();

        let mut rows: Vec<Match> = Vec::with_capacity(fresh.len());
        for match_id in fresh {
            if let Some(row) = self.load(player, match_id, cache)? {
                rows.push(row);
            }
        }
        rows.sort_by_key(|m| m.start_ms); // oldest first: sessions build in order

        let mut inserted = 0;
        for row in &rows {
            if !self.db.insert_match(row)? {
                continue; // raced with another writer; already counted
            }
            inserted += 1;
            log::debug!(
                "ingest: stored match={} player={} queue={} start={} notify={notify}",
                row.match_id,
                player.riot_id(),
                row.queue_id,
                row.start_ms
            );
            if let (true, Some(sessions)) = (notify, &self.sessions)
                && let Err(e) = sessions.on_match(player, row)
            {
                // Stored but never announced would mean it is "known" forever and the
                // notification lost. Un-store it so the next poll tries again.
                self.db.delete_match(&row.match_id, &row.puuid)?;
                return Err(e);
            }
        }

        let newest = rows.iter().map(|r| r.start_ms).max().unwrap_or(0);
        let marker = if first_time { newest.max(started_at) } else { newest };
        if marker != 0 {
            self.db.set_last_seen(&player.puuid, marker)?;
        }
        Ok(inserted)
    }

    fn is_bad(&self, match_id: &str, puuid: &str) -> bool {
        let mut bad = self.bad_matches.lock().unwrap_or_else(|p| p.into_inner());
        let key = (match_id.to_owned(), puuid.to_owned());
        match bad.get(&key) {
            None => false,
            Some(&retry_at) if self.now() >= retry_at => {
                bad.remove(&key);
                false
            }
            Some(_) => true,
        }
    }

    fn mark_bad(&self, match_id: &str, puuid: &str, retry_after_ms: i64) {
        let now = self.now();
        let mut bad = self.bad_matches.lock().unwrap_or_else(|p| p.into_inner());
        if bad.len() > BAD_MATCHES_CAP {
            bad.retain(|_, &mut t| t > now); // bounded: drop what has expired
        }
        bad.insert((match_id.to_owned(), puuid.to_owned()), now + retry_after_ms);
    }

    fn load(&self, player: &Player, match_id: &str, cache: &mut HashMap<String, MatchData>) -> Result<Option<Match>> {
        if !cache.contains_key(match_id) {
            match self.riot.get_match(match_id, &player.platform) {
                Ok(data) => {
                    cache.insert(match_id.to_owned(), data);
                }
                Err(RiotError::NotFound(_)) => {
                    log::warn!("ingest: match {match_id} not found (yet?); will retry later");
                    self.mark_bad(match_id, &player.puuid, NOT_FOUND_RETRY_MS);
                    return Ok(None);
                }
                Err(RiotError::Parse(e)) => {
                    log::warn!("ingest: match {match_id} unparseable ({e}); skipping");
                    self.mark_bad(match_id, &player.puuid, UNUSABLE_RETRY_MS);
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        let data = &cache[match_id];
        let Some(part) = data.participants.get(&player.puuid) else {
            log::warn!("ingest: {} not among participants of {match_id}; skipping", player.riot_id());
            self.mark_bad(match_id, &player.puuid, UNUSABLE_RETRY_MS);
            return Ok(None);
        };
        Ok(Some(Match {
            match_id: match_id.to_owned(),
            puuid: player.puuid.clone(),
            queue_id: data.queue_id,
            champion: part.champion.clone(),
            win: part.win,
            remake: data.remake,
            kills: part.kills,
            deaths: part.deaths,
            assists: part.assists,
            duration_s: data.duration_s,
            start_ms: data.start_ms,
            end_ms: data.end_ms,
            ingested_at: self.now(),
        }))
    }

    /// Silently store the last `days` of history (no notifications).
    pub fn backfill(&self, player: &Player, days: i64) -> Result<usize> {
        let opts = IngestOptions { since_ms: Some(self.now() - days * DAY_MS), notify: Some(false), count: Some(100) };
        self.ingest(player, &mut HashMap::new(), opts)
    }

    // -- names ----------------------------------------------------------------

    /// Re-resolve display names older than a week. The PUUID is the identity.
    pub fn refresh_names(&self) -> Result<usize> {
        let now = self.now();
        let mut changed = 0;
        for player in self.db.list_players(true)? {
            if player.name_checked_at.is_some_and(|t| t != 0 && now - t < NAME_REFRESH_MS) {
                continue;
            }
            let acct = match self.riot.account_by_puuid(&player.puuid, &player.platform) {
                Ok(acct) => acct,
                Err(e) => {
                    log::warn!("name refresh: {} for {}", e.kind(), player.riot_id());
                    continue;
                }
            };
            if (acct.game_name.as_str(), acct.tag_line.as_str())
                != (player.game_name.as_str(), player.tag_line.as_str())
            {
                log::info!("name refresh: {} -> {}#{}", player.riot_id(), acct.game_name, acct.tag_line);
                changed += 1;
            }
            self.db.update_names(&player.puuid, &acct.game_name, &acct.tag_line, now)?;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AccountInfo;
    use crate::notifier::ALERT_COOLDOWN_MS;
    use crate::testutil::{Env, MIN, RecordingSink, T0, make_player_full, match_data};

    const FOREVER: i64 = 1 << 60;

    fn alice_match(id: &str, start: i64) -> MatchData {
        match_data(id, start, 30, &["puuid-alice"])
    }

    fn make_poller(env: &Env) -> (Poller, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let alerter = Arc::new(Alerter::new(sink.clone(), env.db.clone(), vec![42], env.clock.as_clock()));
        (env.poller(Some(alerter)), sink)
    }

    fn stored(env: &Env) -> usize {
        env.db.matches_between("puuid-alice", 0, FOREVER).unwrap().len()
    }

    #[test]
    fn first_time_player_history_is_stored_silently() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.clock.set(T0 + 600 * MIN);
        env.riot.set_ids("puuid-alice", &["EUW1_1", "EUW1_2"]);
        env.riot.add_match(alice_match("EUW1_1", T0));
        env.riot.add_match(alice_match("EUW1_2", T0 + 40 * MIN));

        assert_eq!(poller.poll_once(), 2);
        assert_eq!(stored(&env), 2);
        assert!(env.db.open_sessions(None).unwrap().is_empty()); // nothing announced
        assert!(env.scheduler.calls().is_empty());
        let p = env.db.get_player("puuid-alice").unwrap().unwrap();
        assert!(p.last_seen_ms.unwrap() >= env.clock.now()); // so the *next* game is live
    }

    #[test]
    fn new_game_after_first_poll_is_announced() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.clock.set(T0);
        poller.poll_once(); // first sight, no games
        assert_eq!(env.db.get_player("puuid-alice").unwrap().unwrap().last_seen_ms, Some(T0));

        env.clock.set(T0 + 40 * MIN);
        env.riot.set_ids("puuid-alice", &["EUW1_9"]);
        env.riot.add_match(alice_match("EUW1_9", T0 + 5 * MIN));
        assert_eq!(poller.poll_once(), 1);
        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1);
        assert!(!env.scheduler.calls().is_empty()); // flush job scheduled
    }

    #[test]
    fn ingestion_is_idempotent() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.riot.set_ids("puuid-alice", &["EUW1_9"]);
        env.riot.add_match(alice_match("EUW1_9", T0 + 5 * MIN));

        assert_eq!(poller.poll_once(), 1);
        assert_eq!(poller.poll_once(), 0);
        assert_eq!(poller.poll_once(), 0);
        assert_eq!(env.riot.get_match_calls(), ["EUW1_9"]); // fetched once, never again
        assert_eq!(stored(&env), 1);
        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1); // and announced once
    }

    #[test]
    fn since_is_last_seen_minus_overlap_and_first_time_is_a_week_back() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.clock.set(T0);
        poller.poll_once();
        let (_, since, count) = env.riot.id_calls().last().cloned().unwrap();
        assert_eq!((since, count), (Some(T0 - 7 * DAY_MS), 20));

        poller.poll_once();
        let (_, since, _) = env.riot.id_calls().last().cloned().unwrap();
        assert_eq!(since, Some(T0 - OVERLAP_MS));
    }

    #[test]
    fn a_match_shared_by_two_tracked_players_is_fetched_once() {
        let env = Env::new();
        env.db.add_player(&make_player_full("puuid-bob", "Bob", "EUW")).unwrap();
        let (poller, _) = make_poller(&env);
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.db.set_last_seen("puuid-bob", T0).unwrap();
        env.riot.set_ids("puuid-alice", &["EUW1_5"]);
        env.riot.set_ids("puuid-bob", &["EUW1_5"]);
        env.riot.add_match(match_data("EUW1_5", T0 + MIN, 30, &["puuid-alice", "puuid-bob"]));

        assert_eq!(poller.poll_once(), 2);
        assert_eq!(env.riot.get_match_calls(), ["EUW1_5"]);
    }

    #[test]
    fn missing_or_unusable_matches_are_skipped_not_fatal() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.riot.set_ids("puuid-alice", &["EUW1_404", "EUW1_other", "EUW1_ok"]);
        env.riot.fail_match("EUW1_404", RiotError::NotFound("gone".into()));
        env.riot.add_match(match_data("EUW1_other", T0, 30, &["someone-else"]));
        env.riot.add_match(alice_match("EUW1_ok", T0 + MIN));

        assert_eq!(poller.poll_once(), 1);
        poller.poll_once();
        let calls = env.riot.get_match_calls();
        assert_eq!(calls.iter().filter(|c| *c == "EUW1_404").count(), 1); // not retried every poll
    }

    #[test]
    fn auth_error_alerts_once_per_hour_and_never_fails() {
        let env = Env::new();
        let (poller, sink) = make_poller(&env);
        env.riot.set_error(Some(RiotError::Auth("403".into())));
        for _ in 0..5 {
            assert_eq!(poller.poll_once(), 0);
        }
        let sent = sink.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].0 == 42 && sent[0].1.contains("expired"));

        env.clock.set(T0 + ALERT_COOLDOWN_MS + 1);
        poller.poll_once();
        assert_eq!(sink.sent().len(), 2);
    }

    #[test]
    fn unavailable_riot_is_quiet() {
        let env = Env::new();
        let (poller, sink) = make_poller(&env);
        env.riot.set_error(Some(RiotError::Unavailable("503".into())));
        assert_eq!(poller.poll_once(), 0);
        assert!(sink.sent().is_empty());
    }

    #[test]
    fn a_rate_limit_ends_the_cycle_quietly() {
        let env = Env::new();
        let (poller, sink) = make_poller(&env);
        env.riot
            .set_error(Some(RiotError::RateLimited { message: "429 (server limit)".into(), retry_after: Some(11.0) }));
        assert_eq!(poller.poll_once(), 0);
        assert!(sink.sent().is_empty());
    }

    #[test]
    fn unexpected_failure_alerts_and_does_not_kill_the_loop() {
        let env = Env::new();
        let (poller, sink) = make_poller(&env);
        env.riot.set_error(Some(RiotError::Unexpected("boom".into())));
        assert_eq!(poller.poll_once(), 0); // returned normally
        let sent = sink.sent();
        assert!(sent.len() == 1 && sent[0].1.contains("boom"), "{sent:?}");

        env.riot.set_error(None); // next tick works again
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.riot.set_ids("puuid-alice", &["EUW1_1"]);
        env.riot.add_match(alice_match("EUW1_1", T0 + MIN));
        assert_eq!(poller.poll_once(), 1);
    }

    #[test]
    fn backfill_is_silent_even_for_a_known_player() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.clock.set(T0 + 100 * MIN);
        env.riot.set_ids("puuid-alice", &["EUW1_1"]);
        env.riot.add_match(alice_match("EUW1_1", T0 + MIN));
        let player = env.db.get_player("puuid-alice").unwrap().unwrap();
        assert_eq!(poller.backfill(&player, 14).unwrap(), 1);
        assert!(env.db.open_sessions(None).unwrap().is_empty());
        assert_eq!(env.riot.id_calls().last().unwrap().2, 100); // asks for a full page
    }

    #[test]
    fn refresh_names_updates_stale_display_names() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.riot.set_by_puuid(AccountInfo {
            puuid: "puuid-alice".into(),
            game_name: "NewName".into(),
            tag_line: "NEW".into(),
            summoner_level: None,
        });
        assert_eq!(poller.refresh_names().unwrap(), 1);
        let p = env.db.get_player("puuid-alice").unwrap().unwrap();
        assert_eq!(
            (p.game_name.as_str(), p.tag_line.as_str(), p.name_checked_at),
            ("NewName", "NEW", Some(env.clock.now()))
        );
        assert_eq!(poller.refresh_names().unwrap(), 0); // fresh: not re-checked
    }

    #[test]
    fn a_match_riot_cannot_serve_yet_is_retried_not_blacklisted() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.clock.set(T0 + 60 * MIN);
        env.riot.set_ids("puuid-alice", &["EUW1_9"]);
        env.riot.fail_match("EUW1_9", RiotError::NotFound("details lag the list".into()));

        assert_eq!(poller.poll_once(), 0); // 404: skipped for now
        env.riot.add_match(alice_match("EUW1_9", T0 + 10 * MIN));
        env.clock.set(T0 + 70 * MIN);
        assert_eq!(poller.poll_once(), 0); // not hammered every poll
        assert_eq!(env.riot.get_match_calls().len(), 1);
        env.clock.set(T0 + 100 * MIN); // past the retry delay
        assert_eq!(poller.poll_once(), 1); // picked up, no restart needed
        assert_eq!(stored(&env), 1);
    }

    #[test]
    fn a_notification_failure_unstores_the_match_so_it_is_retried() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.db.set_last_seen("puuid-alice", T0).unwrap();
        env.clock.set(T0 + 60 * MIN);
        env.riot.set_ids("puuid-alice", &["EUW1_1"]);
        env.riot.add_match(alice_match("EUW1_1", T0 + 10 * MIN));

        // Make on_match fail: per_game mode with a notifier whose DB lookup breaks is
        // awkward to stage, so break the sessions table instead.
        env.db.break_sessions_table_for_tests();
        assert_eq!(poller.poll_once(), 0); // poll_once swallows and alerts
        assert_eq!(stored(&env), 0);

        env.db.repair_sessions_table_for_tests();
        assert_eq!(poller.poll_once(), 1);
        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1); // announced after all
    }

    // -- the headline behaviours, end to end --------------------------------------

    #[test]
    fn five_back_to_back_games_send_one_message_after_the_last_not_during_the_fourth() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.clock.set(T0 - 10 * MIN);
        poller.poll_once(); // first sight: history baseline
        let mut ids: Vec<String> = Vec::new();
        let mut sent_before_last = 0;
        let (mut sid, mut end) = (0, 0);
        for i in 0..5 {
            // 25 min games, 3 min queue between
            let start = T0 + i * 28 * MIN;
            let id = format!("EUW1_{i}");
            env.riot.add_match(match_data(&id, start, 25, &["puuid-alice"]));
            ids.insert(0, id);
            env.riot.set_ids("puuid-alice", &ids.iter().map(String::as_str).collect::<Vec<_>>());
            end = start + 25 * MIN;
            env.clock.set(end + 2 * MIN); // poll two minutes after each ends
            poller.poll_once();
            if i < 4 {
                sent_before_last += env.notifier.sessions().len();
            }
            let delay;
            (sid, delay) = env.scheduler.last();
            // The flush check is always "last end + 15 min", so it can't fire mid-session.
            assert_eq!(env.clock.now() + (delay * 1000.0) as i64, end + 15 * MIN);
        }
        assert_eq!(sent_before_last, 0); // silence through game four
        assert_eq!(env.db.open_sessions(None).unwrap().len(), 1);

        env.clock.set(end + 15 * MIN);
        env.service.flush(sid).unwrap();
        let sent = env.notifier.sessions();
        assert_eq!((sent.len(), sent[0].1.len()), (1, 5));
    }

    #[test]
    fn restart_mid_session_still_yields_exactly_one_notification() {
        let env = Env::new();
        let (poller, _) = make_poller(&env);
        env.clock.set(T0 - MIN);
        poller.poll_once();
        env.riot.set_ids("puuid-alice", &["EUW1_1"]);
        env.riot.add_match(alice_match("EUW1_1", T0));
        env.clock.set(T0 + 32 * MIN);
        poller.poll_once();
        let (sid, _) = env.scheduler.last();

        // Crash + restart while the session is open; a *new* poller re-polls the same game.
        env.clock.set(T0 + 50 * MIN);
        let scheduler = Arc::new(crate::testutil::FakeScheduler::default());
        let service = Arc::new(env.make_service(scheduler));
        service.recover().unwrap();
        Poller::new(env.db.clone(), env.riot.clone(), Some(service.clone()), None, env.clock.as_clock()).poll_once();
        service.flush(sid).unwrap();
        service.flush(sid).unwrap();
        assert_eq!(env.notifier.sessions().len(), 1);
    }
}
