//! Shared fakes and builders for the unit tests. Nothing here touches the network.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use jiff::Zoned;
use jiff::civil::date;
use jiff::tz::TimeZone;

use crate::clock::{Clock, Pacer};
use crate::config::{Config, NotificationConfig};
use crate::db::{Database, DbResult};
use crate::flusher::{FlushScheduler, SessionService};
use crate::http::{Http, Response, TransportError};
use crate::models::{AccountInfo, Match, MatchData, ParticipantResult, Player};
use crate::notifier::{AlertSink, Alerter, Notifier, SessionNotifier};
use crate::poller::Poller;
use crate::riot::{Riot, RiotError, RiotResult};
use crate::services::Services;
use crate::telegram::{BotCommand, Chat, Entity, Message, Telegram, TgError, TgResult, Update, User};

/// An arbitrary fixed instant (ms).
pub const T0: i64 = 1_757_000_000_000;
pub const MIN: i64 = 60_000;

pub fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(format!("{name}.json"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn london() -> TimeZone {
    TimeZone::get("Europe/London").unwrap()
}

pub fn zoned(y: i16, m: i8, d: i8, h: i8, mi: i8) -> Zoned {
    date(y, m, d).at(h, mi, 0, 0).to_zoned(london()).unwrap()
}

/// A directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "lolwatch-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn test_db() -> Database {
    Database::open(":memory:").unwrap()
}

// -- builders -------------------------------------------------------------------

pub fn make_player_full(puuid: &str, name: &str, tag: &str) -> Player {
    Player {
        puuid: puuid.into(),
        game_name: name.into(),
        tag_line: tag.into(),
        platform: "euw1".into(),
        active: true,
        added_by: None,
        added_at: T0,
        last_seen_ms: None,
        name_checked_at: None,
    }
}

pub fn make_player() -> Player {
    make_player_full("puuid-alice", "Alice", "EUW")
}

pub fn make_player_named(name: &str, tag: &str) -> Player {
    make_player_full("puuid-alice", name, tag)
}

pub fn make_match(match_id: &str, start_ms: i64, minutes: i64) -> Match {
    Match {
        match_id: match_id.into(),
        puuid: "puuid-alice".into(),
        queue_id: 420,
        champion: "Azir".into(),
        win: 1,
        remake: false,
        kills: Some(5),
        deaths: Some(2),
        assists: Some(7),
        duration_s: minutes * 60,
        start_ms,
        end_ms: start_ms + minutes * MIN,
        ingested_at: T0,
    }
}

pub fn make_match_with(match_id: &str, start_ms: i64, minutes: i64, f: impl FnOnce(&mut Match)) -> Match {
    let mut m = make_match(match_id, start_ms, minutes);
    f(&mut m);
    m
}

pub trait MatchExt {
    fn with_champion(self, champion: &str) -> Self;
    fn with_kda(self, k: i64, d: i64, a: i64) -> Self;
}

impl MatchExt for Match {
    fn with_champion(mut self, champion: &str) -> Self {
        self.champion = champion.into();
        self
    }

    fn with_kda(mut self, k: i64, d: i64, a: i64) -> Self {
        (self.kills, self.deaths, self.assists) = (Some(k), Some(d), Some(a));
        self
    }
}

pub fn match_data(match_id: &str, start_ms: i64, minutes: i64, puuids: &[&str]) -> MatchData {
    let result =
        || ParticipantResult { champion: "Azir".into(), win: 1, kills: Some(5), deaths: Some(2), assists: Some(7) };
    MatchData {
        match_id: match_id.into(),
        queue_id: 420,
        duration_s: minutes * 60,
        start_ms,
        end_ms: start_ms + minutes * MIN,
        remake: false,
        participants: puuids.iter().map(|p| ((*p).to_owned(), result())).collect(),
    }
}

// -- clocks -----------------------------------------------------------------------

/// Frozen, manually advanced wall clock (epoch ms).
#[derive(Clone)]
pub struct TestClock(Arc<AtomicI64>);

impl TestClock {
    pub fn new(now: i64) -> Self {
        Self(Arc::new(AtomicI64::new(now)))
    }

    pub fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }

    pub fn as_clock(&self) -> Clock {
        let inner = self.0.clone();
        Arc::new(move || inner.load(Ordering::SeqCst))
    }
}

/// Monotonic time that only moves when something sleeps.
#[derive(Default)]
pub struct FakePacer {
    state: Mutex<(f64, Vec<f64>)>,
}

impl FakePacer {
    pub fn sleeps(&self) -> Vec<f64> {
        self.state.lock().unwrap().1.clone()
    }
}

impl Pacer for FakePacer {
    fn monotonic_s(&self) -> f64 {
        self.state.lock().unwrap().0
    }

    fn sleep(&self, seconds: f64) {
        let mut state = self.state.lock().unwrap();
        state.0 += seconds;
        state.1.push(seconds);
    }
}

// -- HTTP -------------------------------------------------------------------------

/// Routes `host/path` (Riot hosts by their first label, query stripped) to canned
/// responses; anything unrouted is a 404.
#[derive(Default)]
pub struct FakeHttp {
    routes: Mutex<HashMap<String, Response>>,
    requests: Mutex<Vec<(String, String)>>,
    posts: Mutex<Vec<(String, Vec<u8>)>>,
    fail: Mutex<Option<TransportError>>,
}

impl FakeHttp {
    pub fn with(routes: &[(&str, u16, &str)]) -> Arc<Self> {
        let http = Arc::new(Self::default());
        for (key, status, body) in routes {
            http.route_full(
                key,
                Response { status: *status, retry_after: None, rate_limit_type: None, body: body.as_bytes().to_vec() },
            );
        }
        http
    }

    pub fn route_full(&self, key: &str, response: Response) {
        self.routes.lock().unwrap().insert(key.into(), response);
    }

    pub fn fail_with(&self, error: TransportError) {
        *self.fail.lock().unwrap() = Some(error);
    }

    /// (url, X-Riot-Token) of every GET.
    pub fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }

    pub fn posts(&self) -> Vec<(String, Vec<u8>)> {
        self.posts.lock().unwrap().clone()
    }

    fn key(url: &str) -> String {
        let rest = url.trim_start_matches("https://");
        let rest = rest.split('?').next().unwrap_or(rest);
        rest.replacen(".api.riotgames.com", "", 1)
    }

    fn respond(&self, url: &str) -> Result<Response, TransportError> {
        if let Some(e) = self.fail.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(self.routes.lock().unwrap().get(&Self::key(url)).cloned().unwrap_or(Response {
            status: 404,
            retry_after: None,
            rate_limit_type: None,
            body: br#"{"status": {"status_code": 404}}"#.to_vec(),
        }))
    }
}

impl Http for FakeHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, TransportError> {
        let token =
            headers.iter().find(|(k, _)| *k == "X-Riot-Token").map(|(_, v)| (*v).to_owned()).unwrap_or_default();
        self.requests.lock().unwrap().push((url.into(), token));
        self.respond(url)
    }

    fn post_json(&self, url: &str, body: &[u8]) -> Result<Response, TransportError> {
        self.posts.lock().unwrap().push((url.into(), body.to_vec()));
        self.respond(url)
    }
}

// -- Riot -------------------------------------------------------------------------

#[derive(Default)]
struct RiotState {
    in_game: Option<RiotResult<bool>>,
    in_game_failures: VecDeque<RiotError>,
    in_game_calls: usize,
    ids: HashMap<String, Vec<String>>,
    matches: HashMap<String, MatchData>,
    match_failures: HashMap<String, RiotError>,
    get_match_calls: Vec<String>,
    id_calls: Vec<(String, Option<i64>, u32)>,
    accounts: HashMap<(String, String), AccountInfo>,
    by_puuid: HashMap<String, AccountInfo>,
    error: Option<RiotError>,
}

/// In-memory Riot client.
#[derive(Default)]
pub struct FakeRiot {
    state: Mutex<RiotState>,
}

impl FakeRiot {
    fn with<T>(&self, f: impl FnOnce(&mut RiotState) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    pub fn set_in_game(&self, result: RiotResult<bool>) {
        self.with(|s| s.in_game = Some(result));
    }

    /// Errors returned (in order) before `set_in_game`'s answer.
    pub fn fail_in_game(&self, errors: impl IntoIterator<Item = RiotError>) {
        self.with(|s| s.in_game_failures.extend(errors));
    }

    pub fn in_game_calls(&self) -> usize {
        self.with(|s| s.in_game_calls)
    }

    pub fn set_ids(&self, puuid: &str, ids: &[&str]) {
        self.with(|s| s.ids.insert(puuid.into(), ids.iter().map(|i| (*i).to_owned()).collect()));
    }

    pub fn add_match(&self, m: MatchData) {
        self.with(|s| s.matches.insert(m.match_id.clone(), m));
    }

    /// The next `get_match(id)` fails with `error` (once).
    pub fn fail_match(&self, id: &str, error: RiotError) {
        self.with(|s| s.match_failures.insert(id.into(), error));
    }

    pub fn get_match_calls(&self) -> Vec<String> {
        self.with(|s| s.get_match_calls.clone())
    }

    pub fn id_calls(&self) -> Vec<(String, Option<i64>, u32)> {
        self.with(|s| s.id_calls.clone())
    }

    pub fn set_account(&self, name: &str, tag: &str, acct: AccountInfo) {
        self.with(|s| s.accounts.insert((name.to_lowercase(), tag.to_lowercase()), acct));
    }

    pub fn set_by_puuid(&self, acct: AccountInfo) {
        self.with(|s| s.by_puuid.insert(acct.puuid.clone(), acct));
    }

    /// Fails `resolve_account` and `recent_match_ids` while set.
    pub fn set_error(&self, error: Option<RiotError>) {
        self.with(|s| s.error = error);
    }
}

impl Riot for FakeRiot {
    fn resolve_account(&self, game_name: &str, tag_line: &str, _platform: &str) -> RiotResult<AccountInfo> {
        self.with(|s| {
            if let Some(e) = &s.error {
                return Err(e.clone());
            }
            s.accounts
                .get(&(game_name.to_lowercase(), tag_line.to_lowercase()))
                .cloned()
                .ok_or_else(|| RiotError::NotFound("no such Riot ID".into()))
        })
    }

    fn account_by_puuid(&self, puuid: &str, _platform: &str) -> RiotResult<AccountInfo> {
        self.with(|s| s.by_puuid.get(puuid).cloned().ok_or_else(|| RiotError::NotFound("no such PUUID".into())))
    }

    fn recent_match_ids(
        &self,
        puuid: &str,
        _platform: &str,
        since_ms: Option<i64>,
        count: u32,
    ) -> RiotResult<Vec<String>> {
        self.with(|s| {
            if let Some(e) = &s.error {
                return Err(e.clone());
            }
            s.id_calls.push((puuid.into(), since_ms, count));
            Ok(s.ids.get(puuid).cloned().unwrap_or_default())
        })
    }

    fn get_match(&self, match_id: &str, _platform: &str) -> RiotResult<MatchData> {
        self.with(|s| {
            s.get_match_calls.push(match_id.into());
            if let Some(e) = s.match_failures.remove(match_id) {
                return Err(e);
            }
            s.matches.get(match_id).cloned().ok_or_else(|| RiotError::NotFound(format!("no match {match_id}")))
        })
    }

    fn is_in_game(&self, _puuid: &str, _platform: &str) -> RiotResult<bool> {
        self.with(|s| {
            s.in_game_calls += 1;
            if let Some(e) = s.in_game_failures.pop_front() {
                return Err(e);
            }
            s.in_game.clone().unwrap_or(Ok(false))
        })
    }
}

// -- scheduler / notifier / alerts -----------------------------------------------------

#[derive(Default)]
pub struct FakeScheduler {
    calls: Mutex<Vec<(i64, f64)>>,
}

impl FakeScheduler {
    pub fn calls(&self) -> Vec<(i64, f64)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn last(&self) -> (i64, f64) {
        *self.calls.lock().unwrap().last().expect("something was scheduled")
    }
}

impl FlushScheduler for FakeScheduler {
    fn schedule(&self, session_id: i64, delay_s: f64) {
        self.calls.lock().unwrap().push((session_id, delay_s));
    }
}

pub struct FakeNotifier {
    ok: Mutex<bool>,
    sessions: Mutex<Vec<(Player, Vec<Match>)>>,
    games: Mutex<Vec<(Player, Match)>>,
}

impl Default for FakeNotifier {
    fn default() -> Self {
        Self { ok: Mutex::new(true), sessions: Mutex::default(), games: Mutex::default() }
    }
}

impl FakeNotifier {
    pub fn set_ok(&self, ok: bool) {
        *self.ok.lock().unwrap() = ok;
    }

    pub fn sessions(&self) -> Vec<(Player, Vec<Match>)> {
        self.sessions.lock().unwrap().clone()
    }

    pub fn games(&self) -> Vec<(Player, Match)> {
        self.games.lock().unwrap().clone()
    }
}

impl SessionNotifier for FakeNotifier {
    fn send_session(&self, player: &Player, matches: &[Match]) -> DbResult<bool> {
        let ok = *self.ok.lock().unwrap();
        if ok {
            self.sessions.lock().unwrap().push((player.clone(), matches.to_vec()));
        }
        Ok(ok)
    }

    fn send_game(&self, player: &Player, m: &Match) -> DbResult<bool> {
        let ok = *self.ok.lock().unwrap();
        if ok {
            self.games.lock().unwrap().push((player.clone(), m.clone()));
        }
        Ok(ok)
    }
}

#[derive(Default)]
pub struct RecordingSink {
    sent: Mutex<Vec<(i64, String)>>,
}

impl RecordingSink {
    pub fn sent(&self) -> Vec<(i64, String)> {
        self.sent.lock().unwrap().clone()
    }
}

impl AlertSink for RecordingSink {
    fn send_to(&self, chat_id: i64, text: &str) -> bool {
        self.sent.lock().unwrap().push((chat_id, text.into()));
        true
    }
}

// -- Telegram ------------------------------------------------------------------------

#[derive(Default)]
pub struct FakeTelegram {
    sent: Mutex<Vec<(i64, String)>>,
    fail_next: Mutex<VecDeque<TgError>>,
    fail_chats: Mutex<HashSet<i64>>,
    menus: Mutex<Vec<(Option<i64>, Vec<BotCommand>)>>,
    pub updates: Mutex<VecDeque<Vec<Update>>>,
    pub me_error: Mutex<Option<TgError>>,
}

impl FakeTelegram {
    pub fn sent(&self) -> Vec<(i64, String)> {
        self.sent.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.sent.lock().unwrap().clear();
    }

    pub fn fail_next(&self, errors: impl IntoIterator<Item = TgError>) {
        self.fail_next.lock().unwrap().extend(errors);
    }

    pub fn fail_chat(&self, chat_id: i64) {
        self.fail_chats.lock().unwrap().insert(chat_id);
    }

    pub fn menus(&self) -> Vec<(Option<i64>, Vec<BotCommand>)> {
        self.menus.lock().unwrap().clone()
    }
}

impl Telegram for FakeTelegram {
    fn get_me(&self) -> TgResult<User> {
        match self.me_error.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(User { id: 1000, is_bot: true, username: Some("tb".into()) }),
        }
    }

    fn get_updates(&self, _offset: i64, _timeout_s: u32) -> TgResult<Vec<Update>> {
        Ok(self.updates.lock().unwrap().pop_front().unwrap_or_default())
    }

    fn send_message(&self, chat_id: i64, text: &str, _html: bool) -> TgResult<()> {
        if let Some(e) = self.fail_next.lock().unwrap().pop_front() {
            return Err(e);
        }
        if self.fail_chats.lock().unwrap().contains(&chat_id) {
            return Err(TgError::Forbidden("bot was kicked".into()));
        }
        self.sent.lock().unwrap().push((chat_id, text.into()));
        Ok(())
    }

    fn set_my_commands(&self, commands: &[BotCommand], chat_id: Option<i64>) -> TgResult<()> {
        self.menus.lock().unwrap().push((chat_id, commands.to_vec()));
        Ok(())
    }

    fn drop_pending_updates(&self) -> TgResult<()> {
        Ok(())
    }
}

// -- wired environments -------------------------------------------------------------

/// A SessionService over a real (in-memory) SQLite DB and fakes, with Alice tracked.
pub struct Env {
    pub db: Arc<Database>,
    pub clock: TestClock,
    pub riot: Arc<FakeRiot>,
    pub notifier: Arc<FakeNotifier>,
    pub scheduler: Arc<FakeScheduler>,
    pub cfg: NotificationConfig,
    pub service: Arc<SessionService>,
}

impl Env {
    pub fn new() -> Self {
        Self::with_cfg(NotificationConfig::default())
    }

    pub fn with_cfg(cfg: NotificationConfig) -> Self {
        let db = Arc::new(test_db());
        db.add_player(&make_player()).unwrap();
        let clock = TestClock::new(T0);
        let riot = Arc::new(FakeRiot::default());
        let notifier = Arc::new(FakeNotifier::default());
        let scheduler = Arc::new(FakeScheduler::default());
        let service = Arc::new(SessionService::new(
            db.clone(),
            riot.clone(),
            notifier.clone(),
            scheduler.clone(),
            cfg.clone(),
            clock.as_clock(),
        ));
        Self { db, clock, riot, notifier, scheduler, cfg, service }
    }

    pub fn make_service(&self, scheduler: Arc<FakeScheduler>) -> SessionService {
        SessionService::new(
            self.db.clone(),
            self.riot.clone(),
            self.notifier.clone(),
            scheduler,
            self.cfg.clone(),
            self.clock.as_clock(),
        )
    }

    /// Mimic the poller: store, then hand to the session service.
    pub fn ingest(&self, m: Match) {
        assert!(self.db.insert_match(&m).unwrap());
        self.service.on_match(&make_player(), &m).unwrap();
    }

    pub fn poller(&self, alerter: Option<Arc<Alerter>>) -> Poller {
        Poller::new(self.db.clone(), self.riot.clone(), Some(self.service.clone()), alerter, self.clock.as_clock())
    }
}

/// Full command-handling services over fakes.
pub struct BotEnv {
    pub svc: Services,
    pub riot: Arc<FakeRiot>,
    pub tg: Arc<FakeTelegram>,
}

impl BotEnv {
    pub fn new(admins: &[i64]) -> Self {
        let env = Env::new();
        let tg = Arc::new(FakeTelegram::default());
        let pacer = Arc::new(FakePacer::default());
        let notifier = Arc::new(Notifier::new(tg.clone(), env.db.clone(), pacer));
        let alerter = Arc::new(Alerter::new(notifier.clone(), env.db.clone(), admins.to_vec(), env.clock.as_clock()));
        let poller = Arc::new(env.poller(None));
        let svc = Services {
            cfg: Config::for_tests(admins),
            db: env.db.clone(),
            riot: env.riot.clone(),
            poller,
            sessions: env.service.clone(),
            notifier,
            alerter,
            tg: tg.clone(),
            clock: crate::clock::system_clock(),
            next_weekly: Mutex::new(None),
            logged_strangers: Mutex::new(HashSet::new()),
        };
        Self { svc, riot: env.riot, tg }
    }

    pub fn message(text: &str, user: i64, chat: i64, kind: &str) -> Message {
        let command_len = if text.starts_with('/') { text.find(' ').unwrap_or(text.len()) } else { 0 };
        Message {
            from: Some(User { id: user, is_bot: false, username: None }),
            sender_chat: None,
            chat: Chat { id: chat, kind: kind.into() },
            text: Some(text.into()),
            entities: if command_len > 0 {
                vec![Entity { kind: "bot_command".into(), offset: 0, length: command_len }]
            } else {
                vec![]
            },
            reply_to_message: None,
            migrate_from_chat_id: None,
        }
    }

    /// Deliver one message as an update; returns what the bot said in that chat.
    pub fn deliver(&self, msg: Message) -> Vec<String> {
        self.tg.clear();
        let chat = msg.chat.id;
        crate::bot::handle_update(&self.svc, &Update { update_id: 1, message: Some(msg) }, "tb");
        self.tg.sent().into_iter().filter(|(c, _)| *c == chat).map(|(_, t)| t).collect()
    }

    pub fn call_raw(&self, text: &str, user: i64, chat: i64, kind: &str) -> Vec<String> {
        self.deliver(Self::message(text, user, chat, kind))
    }

    pub fn call(&self, command: &str, args: &str, user: i64, chat: i64, kind: &str) -> Vec<String> {
        let text = if args.is_empty() { format!("/{command}") } else { format!("/{command} {args}") };
        self.call_raw(&text, user, chat, kind)
    }

    /// Everything sent anywhere since the last delivery.
    pub fn replies(&self) -> Vec<String> {
        self.tg.sent().into_iter().map(|(_, t)| t).collect()
    }
}
