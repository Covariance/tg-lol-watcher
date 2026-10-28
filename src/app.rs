//! Application wiring: build services, start-up checks, the job runner and the
//! Telegram long-poll loop. Two threads in total: this one runs jobs, the other
//! reads Telegram.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;

use crate::bot::{handle_update, publish_menu};
use crate::clock::{Clock, Pacer, RealPacer, system_clock};
use crate::config::Config;
use crate::db::{Database, key_fingerprint};
use crate::flusher::SessionService;
use crate::http::UreqHttp;
use crate::notifier::{Alerter, Notifier};
use crate::poller::Poller;
use crate::reports::broadcast_weekly;
use crate::riot::client::{APP_RATE_LIMITS, HttpRiotClient, RateLimiter};
use crate::riot::retry::RetryingClient;
use crate::riot::{Riot, RiotError};
use crate::scheduler::{Job, Scheduler, next_repeat, next_weekly};
use crate::services::Services;
use crate::telegram::{BotApi, Telegram, TgError};

pub const KEY_FP: &str = "riot_key_fingerprint";
const NAMES_INTERVAL_MS: i64 = 6 * 3600 * 1000;
const RIOT_TIMEOUT: Duration = Duration::from_secs(30);
/// getUpdates holds the connection open this long when there is nothing to say.
const LONG_POLL_S: u32 = 50;

/// A start-up check failed; the process should exit non-zero.
#[derive(Debug)]
pub struct StartupError(pub String);

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Point the clients somewhere else (a mock server, a local Bot API server).
const RIOT_BASE_ENV: &str = "LOLWATCH_RIOT_BASE_URL";
const TELEGRAM_BASE_ENV: &str = "LOLWATCH_TELEGRAM_BASE_URL";

fn env_url(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn riot_base_url() -> Option<String> {
    env_url(RIOT_BASE_ENV)
}

/// The production Riot client: HTTP, client-side rate limits, retries.
pub fn riot_client(api_key: &str, pacer: Arc<dyn Pacer>) -> Arc<dyn Riot> {
    let http = Arc::new(UreqHttp::new(RIOT_TIMEOUT));
    let limiter = RateLimiter::new(APP_RATE_LIMITS, pacer.clone());
    let client = HttpRiotClient::new(api_key, http, limiter).with_base_url(riot_base_url());
    Arc::new(RetryingClient::new(Arc::new(client), pacer))
}

/// Wire every service around the given outside world.
pub fn build(
    cfg: Config,
    db: Arc<Database>,
    riot: Arc<dyn Riot>,
    tg: Arc<dyn Telegram>,
    pacer: Arc<dyn Pacer>,
    clock: Clock,
) -> (Services, Arc<Scheduler>) {
    let scheduler = Arc::new(Scheduler::new(clock.clone()));
    let notifier = Arc::new(Notifier::new(tg.clone(), db.clone(), pacer));
    let alerter = Arc::new(Alerter::new(notifier.clone(), db.clone(), cfg.admin_user_ids.clone(), clock.clone()));
    let sessions = Arc::new(SessionService::new(
        db.clone(),
        riot.clone(),
        notifier.clone(),
        scheduler.clone(),
        cfg.notifications.clone(),
        clock.clone(),
    ));
    let poller =
        Arc::new(Poller::new(db.clone(), riot.clone(), Some(sessions.clone()), Some(alerter.clone()), clock.clone()));
    let svc = Services {
        cfg,
        db,
        riot,
        poller,
        sessions,
        notifier,
        alerter,
        tg,
        clock,
        next_weekly: Mutex::new(None),
        logged_strangers: Mutex::new(HashSet::new()),
    };
    (svc, scheduler)
}

// -- start-up checks ----------------------------------------------------------

/// PUUIDs are encrypted per key. A changed key with stored PUUIDs means every one of
/// them is silently invalid until `repuuid` runs.
pub fn check_key_change(svc: &Services) -> crate::error::Result<()> {
    let current = key_fingerprint(&svc.cfg.riot_api_key);
    let stored = svc.db.kv_get(KEY_FP)?;
    match stored {
        Some(stored) if !svc.db.list_players(false)?.is_empty() => {
            if stored != current {
                let msg = "The Riot API key has changed since the last run. Stored PUUIDs are encrypted \
                           per key and are now INVALID: run `lolwatch repuuid`, then restart.";
                log::error!("{msg}");
                svc.alerter.alert("key_changed", msg);
            }
        }
        _ => svc.db.kv_set(KEY_FP, &current)?,
    }
    Ok(())
}

/// One cheap call to prove the key works. Only an auth failure is fatal.
pub fn validate_riot(svc: &Services) -> Result<(), StartupError> {
    let players = svc.db.list_players(true).map_err(|e| StartupError(format!("database: {e}")))?;
    let Some(p) = players.first() else {
        log::warn!("startup: no players yet, so the Riot key could not be validated");
        return Ok(());
    };
    match svc.riot.resolve_account(&p.game_name, &p.tag_line, &p.platform) {
        Ok(_) => Ok(()),
        Err(RiotError::Auth(e)) => Err(StartupError(format!("Riot API key rejected (403): {e}. Is it expired?"))),
        Err(e) => {
            log::warn!("startup: Riot validation inconclusive ({}: {e}); continuing", e.kind());
            Ok(())
        }
    }
}

/// Everything between "the process started" and "jobs are scheduled".
pub fn start(svc: &Services, scheduler: &Scheduler) -> Result<(), StartupError> {
    validate_riot(svc)?;
    check_key_change(svc).map_err(|e| StartupError(format!("database: {e}")))?;
    let recovered = svc.sessions.recover().map_err(|e| StartupError(format!("database: {e}")))?;
    log::info!("recovered {recovered} open session(s)");

    scheduler.after(5_000, Job::Poll);
    scheduler.after(120_000, Job::Names);
    if svc.cfg.weekly.enabled {
        schedule_weekly(svc, scheduler);
        let w = &svc.cfg.weekly;
        log::info!("weekly summary scheduled: {} {:02}:{:02} {}", w.day, w.hour, w.minute, w.timezone);
    }
    let players = svc.db.list_players(true).map(|p| p.len()).unwrap_or(0);
    log::info!(
        "started: mode={} poll={}s players={players}",
        svc.cfg.notifications.mode.as_str(),
        svc.cfg.poll_interval_seconds
    );
    Ok(())
}

fn schedule_weekly(svc: &Services, scheduler: &Scheduler) {
    let w = &svc.cfg.weekly;
    let now = Timestamp::from_millisecond(scheduler.now()).unwrap_or_else(|_| Timestamp::now());
    let next = next_weekly(now, &w.tz, w.weekday(), w.hour, w.minute);
    scheduler.at(next.timestamp().as_millisecond(), Job::Weekly);
    *svc.next_weekly.lock().unwrap_or_else(|p| p.into_inner()) = Some(next);
}

// -- jobs ---------------------------------------------------------------------

/// Run one due job and queue whatever comes after it. Never fails.
pub fn run_job(svc: &Services, scheduler: &Scheduler, due: i64, job: Job) {
    match job {
        Job::Poll => {
            svc.poller.poll_once(); // never fails
            let interval = svc.cfg.poll_interval_seconds * 1000;
            scheduler.at(next_repeat(due, interval, scheduler.now()), Job::Poll);
        }
        Job::Names => {
            if let Err(e) = svc.poller.refresh_names() {
                log::error!("name refresh failed: {e}");
            }
            scheduler.at(next_repeat(due, NAMES_INTERVAL_MS, scheduler.now()), Job::Names);
        }
        Job::Flush(session_id) => {
            if let Err(e) = svc.sessions.flush(session_id) {
                // A bug or DB hiccup must not lose the notification: try again shortly.
                log::error!("session={session_id} flush job crashed ({e}); retrying");
                svc.sessions.retry_flush_soon(session_id);
                svc.alerter.alert(
                    "flush_error",
                    &format!("Flushing session {session_id} failed unexpectedly; will retry. See the logs."),
                );
            }
        }
        Job::Weekly => {
            let now = Timestamp::from_millisecond(scheduler.now()).unwrap_or_else(|_| Timestamp::now());
            match broadcast_weekly(&svc.db, &svc.cfg, &svc.notifier, now.to_zoned(svc.cfg.tz().clone())) {
                Ok((n, delivered)) => {
                    log::info!("weekly summary: {n} message(s), {delivered} delivered");
                    if delivered < n {
                        svc.alerter.alert(
                            "weekly_delivery",
                            &format!(
                                "The weekly summary could not be delivered ({delivered} of {n} messages). See the logs."
                            ),
                        );
                    }
                }
                Err(e) => {
                    log::error!("weekly summary failed: {e}");
                    svc.alerter.alert("weekly_error", "The weekly summary failed. See the logs.");
                }
            }
            schedule_weekly(svc, scheduler);
        }
    }
}

/// Read Telegram forever, one update at a time (like python-telegram-bot's default).
fn poll_telegram(svc: &Services, username: &str) {
    if let Err(e) = svc.tg.drop_pending_updates() {
        log::warn!("could not drop pending updates: {e}");
    }
    let mut offset = 0;
    let mut backoff = 1.0_f64;
    loop {
        match svc.tg.get_updates(offset, LONG_POLL_S) {
            Ok(updates) => {
                backoff = 1.0;
                for update in updates {
                    offset = offset.max(update.update_id + 1);
                    handle_update(svc, &update, username);
                }
            }
            Err(TgError::RetryAfter(seconds)) => std::thread::sleep(Duration::from_secs_f64(seconds)),
            Err(e) => {
                log::warn!("telegram getUpdates failed: {e}; retrying in {backoff:.0}s");
                std::thread::sleep(Duration::from_secs_f64(backoff));
                backoff = (backoff * 2.0).min(30.0);
            }
        }
    }
}

pub fn run(cfg: Config) -> Result<(), StartupError> {
    let pacer: Arc<dyn Pacer> = Arc::new(RealPacer::default());
    // Long enough for a long poll plus a slow network.
    let tg: Arc<dyn Telegram> = Arc::new(BotApi::with_base_url(
        &cfg.telegram_bot_token,
        Arc::new(UreqHttp::new(Duration::from_secs(LONG_POLL_S as u64 + 25))),
        env_url(TELEGRAM_BASE_ENV).as_deref(),
    ));
    let me = tg.get_me().map_err(|e| StartupError(format!("Telegram token validation failed: {e}")))?;
    let username = me.username.unwrap_or_default();
    log::info!("telegram: connected as @{username}");

    let db =
        Arc::new(Database::open(&cfg.db_path).map_err(|e| StartupError(format!("database {}: {e}", cfg.db_path)))?);
    let riot = riot_client(&cfg.riot_api_key, pacer.clone());
    let (svc, scheduler) = build(cfg, db, riot, tg, pacer, system_clock());
    publish_menu(&svc);
    start(&svc, &scheduler)?;

    let svc = Arc::new(svc);
    let reader = svc.clone();
    std::thread::Builder::new()
        .name("telegram".into())
        .stack_size(1024 * 1024)
        .spawn(move || poll_telegram(&reader, &username))
        .map_err(|e| StartupError(format!("could not start the Telegram thread: {e}")))?;

    while let Some((due, job)) = scheduler.next() {
        run_job(&svc, &scheduler, due, job);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakePacer, FakeRiot, FakeTelegram, T0, TestClock, make_match, make_player, test_db};

    struct World {
        svc: Services,
        scheduler: Arc<Scheduler>,
        riot: Arc<FakeRiot>,
        tg: Arc<FakeTelegram>,
        clock: TestClock,
    }

    fn world_with(db: Arc<Database>, key: &str) -> World {
        let (riot, tg, clock) = (Arc::new(FakeRiot::default()), Arc::new(FakeTelegram::default()), TestClock::new(T0));
        let mut cfg = Config::for_tests(&[777]);
        cfg.riot_api_key = key.into();
        let (svc, scheduler) =
            build(cfg, db, riot.clone(), tg.clone(), Arc::new(FakePacer::default()), clock.as_clock());
        World { svc, scheduler, riot, tg, clock }
    }

    fn world() -> World {
        world_with(Arc::new(test_db()), "RGAPI-one")
    }

    #[test]
    fn startup_registers_jobs_and_records_the_key() {
        let w = world();
        start(&w.svc, &w.scheduler).unwrap();
        let jobs: Vec<Job> = w.scheduler.pending().into_iter().map(|(_, j)| j).collect();
        assert_eq!(jobs, [Job::Poll, Job::Names, Job::Weekly]);
        assert_eq!(w.svc.db.kv_get(KEY_FP).unwrap(), Some(key_fingerprint("RGAPI-one")));
        assert!(w.svc.next_weekly.lock().unwrap().is_some());
    }

    #[test]
    fn startup_publishes_the_command_menu() {
        let w = world();
        publish_menu(&w.svc);
        let menus = w.tg.menus();
        assert_eq!(menus.len(), 2);
        assert!(menus[0].0.is_none() && menus[1].0 == Some(777));
        assert!(menus[1].1.len() > menus[0].1.len());
    }

    #[test]
    fn restart_reschedules_open_sessions() {
        let db = Arc::new(test_db());
        db.add_player(&make_player()).unwrap();
        db.insert_match(&make_match("m1", T0, 30)).unwrap();
        let sid = db.create_session("puuid-alice", T0, T0 + 1_800_000, T0 - 1).unwrap(); // overdue
        db.attach_match(sid, "m1", "puuid-alice").unwrap();
        let w = world_with(db, "RGAPI-one");
        start(&w.svc, &w.scheduler).unwrap();
        assert!(w.scheduler.pending().contains(&(T0, Job::Flush(sid))));
    }

    #[test]
    fn a_changed_api_key_triggers_a_loud_repuuid_warning() {
        let db = Arc::new(test_db());
        db.add_player(&make_player()).unwrap();
        db.kv_set(KEY_FP, &key_fingerprint("RGAPI-old")).unwrap();
        let w = world_with(db, "RGAPI-new");
        start(&w.svc, &w.scheduler).unwrap();
        let alerts = w.tg.sent();
        assert!(alerts.iter().any(|(chat, text)| *chat == 777 && text.contains("repuuid")), "{alerts:?}");
        assert_eq!(w.svc.db.kv_get(KEY_FP).unwrap(), Some(key_fingerprint("RGAPI-old"))); // not "fixed"
    }

    #[test]
    fn startup_refuses_when_validation_call_is_rejected() {
        let db = Arc::new(test_db());
        db.add_player(&make_player()).unwrap();
        let w = world_with(db, "RGAPI-one");
        w.riot.set_error(Some(RiotError::Auth("403".into())));
        let err = start(&w.svc, &w.scheduler).unwrap_err();
        assert!(err.0.contains("403"));
    }

    #[test]
    fn a_riot_outage_at_startup_is_not_fatal() {
        let db = Arc::new(test_db());
        db.add_player(&make_player()).unwrap();
        let w = world_with(db, "RGAPI-one");
        w.riot.set_error(Some(RiotError::Unavailable("503".into())));
        assert!(start(&w.svc, &w.scheduler).is_ok());
    }

    #[test]
    fn the_poll_job_repeats_at_a_fixed_rate_even_when_riot_explodes() {
        let db = Arc::new(test_db());
        db.add_player(&make_player()).unwrap();
        let w = world_with(db, "RGAPI-one");
        w.riot.set_error(Some(RiotError::Unexpected("boom".into())));
        run_job(&w.svc, &w.scheduler, T0, Job::Poll);
        assert!(w.scheduler.pending().contains(&(T0 + 120_000, Job::Poll)));
        assert!(w.tg.sent().iter().any(|(_, t)| t.contains("Poller error")));
    }

    #[test]
    fn the_weekly_job_sends_and_reschedules_itself_a_week_later() {
        let w = world();
        w.svc.db.add_subscriber(-5, 0).unwrap();
        w.clock.set(crate::testutil::zoned(2026, 9, 20, 20, 0).timestamp().as_millisecond());
        run_job(&w.svc, &w.scheduler, w.clock.now(), Job::Weekly);
        assert!(w.tg.sent().iter().any(|(c, t)| *c == -5 && t.contains("Week ending 20 Sep")));
        let next = w.svc.next_weekly.lock().unwrap().clone().unwrap();
        assert_eq!((next.day(), next.hour()), (27, 20));
    }

    #[test]
    fn the_weekly_job_alerts_the_admin_when_nothing_was_delivered() {
        let w = world();
        w.svc.db.add_subscriber(-5, 0).unwrap();
        w.tg.fail_chat(-5);
        run_job(&w.svc, &w.scheduler, T0, Job::Weekly);
        assert!(w.tg.sent().iter().any(|(c, t)| *c == 777 && t.contains("could not be delivered")));
    }

    #[test]
    fn a_crashing_flush_job_is_retried_and_reported() {
        let w = world();
        w.svc.db.break_sessions_table_for_tests();
        run_job(&w.svc, &w.scheduler, T0, Job::Flush(9));
        assert!(w.scheduler.pending().iter().any(|(_, j)| *j == Job::Flush(9)));
        assert!(w.tg.sent().iter().any(|(c, t)| *c == 777 && t.contains("Flushing session 9")));
    }
}
