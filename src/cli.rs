//! Operator commands: `lolwatch <command>`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::app::{KEY_FP, riot_client};
use crate::clock::{Clock, RealPacer, system_clock};
use crate::config::{Config, parse_riot_id};
use crate::db::{Database, key_fingerprint};
use crate::error::{Error, Result};
use crate::poller::Poller;
use crate::reports::render_window;
use crate::riot::{Riot, RiotError, is_platform, platforms};

pub const USAGE: &str = "\
lolwatch — League of Legends watcher for Telegram

usage: lolwatch [command]

  (none) | run                      run the bot
  resolve GameName#TAG [--platform P]
                                    look up a Riot ID end to end (key + account-v1)
  backfill [--days N]               silently store the last N days (default 14) for every player
  repuuid                           re-resolve every PUUID after an API key change
  dry-run-summary [--days N]        print the weekly summary to the terminal; nothing is sent
  smoke GameName#TAG [platform]     check the Riot client against the live API
  help | --help                     this text
  version | --version
";

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run,
    Resolve { riot_id: String, platform: Option<String> },
    Backfill { days: i64 },
    Repuuid,
    DryRunSummary { days: i64 },
    Smoke { riot_id: String, platform: Option<String> },
    Help,
    Version,
}

fn flag_days(args: &[String], default: i64) -> std::result::Result<i64, String> {
    match args {
        [] => Ok(default),
        [flag, n] if flag == "--days" => n.parse().map_err(|_| format!("--days: not a number: {n:?}")),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

fn platform_arg(p: &str) -> std::result::Result<String, String> {
    if is_platform(p) {
        Ok(p.to_lowercase())
    } else {
        Err(format!("unknown platform {p:?} (one of: {})", platforms().collect::<Vec<_>>().join(", ")))
    }
}

pub fn parse_args(args: &[String]) -> std::result::Result<Command, String> {
    let Some((head, rest)) = args.split_first() else {
        return Ok(Command::Run);
    };
    Ok(match (head.as_str(), rest) {
        ("run", []) => Command::Run,
        ("help" | "--help" | "-h", _) => Command::Help,
        ("version" | "--version" | "-V", _) => Command::Version,
        ("resolve", [id]) => Command::Resolve { riot_id: id.clone(), platform: None },
        ("resolve", [id, flag, p]) if flag == "--platform" => {
            Command::Resolve { riot_id: id.clone(), platform: Some(platform_arg(p)?) }
        }
        ("backfill", rest) => Command::Backfill { days: flag_days(rest, 14)? },
        ("repuuid", []) => Command::Repuuid,
        ("dry-run-summary", rest) => Command::DryRunSummary { days: flag_days(rest, 7)? },
        ("smoke", [id]) => Command::Smoke { riot_id: id.clone(), platform: None },
        ("smoke", [id, p]) => Command::Smoke { riot_id: id.clone(), platform: Some(platform_arg(p)?) },
        _ => return Err(format!("unknown command: {}", args.join(" "))),
    })
}

/// What the commands need: the config, a Riot client and a clock.
pub struct CliEnv {
    pub cfg: Config,
    pub riot: Arc<dyn Riot>,
    pub clock: Clock,
}

impl CliEnv {
    pub fn real(cfg: Config) -> Self {
        let riot = riot_client(&cfg.riot_api_key, Arc::new(RealPacer::default()));
        Self { cfg, riot, clock: system_clock() }
    }

    fn open_db(&self) -> Result<Database> {
        Ok(Database::open(&self.cfg.db_path)?)
    }
}

pub fn resolve(env: &CliEnv, riot_id: &str, platform: Option<&str>) -> Result<i32> {
    let Some((name, tag)) = parse_riot_id(riot_id) else {
        eprintln!("expected GameName#TAG, got {riot_id:?}");
        return Ok(2);
    };
    let platform = platform.unwrap_or(&env.cfg.default_platform);
    match env.riot.resolve_account(&name, &tag, platform) {
        Err(RiotError::NotFound(_)) => {
            eprintln!("not found: {name}#{tag} on {platform}");
            Ok(1)
        }
        Err(e) => Err(e.into()),
        Ok(acct) => {
            let level = acct.summoner_level.map_or("None".into(), |l| l.to_string());
            println!("{}#{}  platform={platform}  level={level}\npuuid={}", acct.game_name, acct.tag_line, acct.puuid);
            Ok(0)
        }
    }
}

pub fn backfill(env: &CliEnv, days: i64) -> Result<i32> {
    let db = Arc::new(env.open_db()?);
    let poller = Poller::new(db.clone(), env.riot.clone(), None, None, env.clock.clone());
    let players = db.list_players(true)?;
    if players.is_empty() {
        println!("no players in the database (add one with /add in Telegram)");
    }
    for p in &players {
        let n = poller.backfill(p, days)?;
        println!("{}: {n} new match(es) stored", p.riot_id());
    }
    Ok(0)
}

/// Re-resolve every player's PUUID under the *current* key and cascade it.
pub fn repuuid(env: &CliEnv) -> Result<i32> {
    let db = env.open_db()?;
    let mut failures = 0;
    for p in db.list_players(false)? {
        let acct = match env.riot.resolve_account(&p.game_name, &p.tag_line, &p.platform) {
            Ok(acct) => acct,
            Err(RiotError::NotFound(_)) => {
                eprintln!("{}: NOT FOUND (renamed?); left unchanged", p.riot_id());
                failures += 1;
                continue;
            }
            Err(RiotError::Auth(e)) => {
                eprintln!("Riot rejected the key: {e}");
                return Ok(1);
            }
            Err(e) => return Err(e.into()),
        };
        if acct.puuid == p.puuid {
            println!("{}: unchanged", p.riot_id());
        } else if db.get_player(&acct.puuid)?.is_some() {
            eprintln!("{}: new PUUID already exists in the DB; skipping", p.riot_id());
            failures += 1;
        } else {
            db.rewrite_puuid(&p.puuid, &acct.puuid)?;
            println!("{}: PUUID rewritten", p.riot_id());
        }
    }
    if failures == 0 {
        db.kv_set(KEY_FP, &key_fingerprint(&env.cfg.riot_api_key))?;
        println!("done; key fingerprint recorded (start-up warning cleared)");
        Ok(0)
    } else {
        eprintln!("{failures} player(s) need attention; fingerprint NOT updated");
        Ok(1)
    }
}

/// The text a person reads in a terminal: tags dropped, entities kept as Telegram would.
pub fn strip_tags(text: &str) -> String {
    ["<b>", "</b>", "<pre>", "</pre>"].iter().fold(text.to_owned(), |t, tag| t.replace(tag, ""))
}

pub fn dry_run_summary(env: &CliEnv, days: i64) -> Result<String> {
    let db = env.open_db()?;
    let players = db.list_players(true)?;
    let now = jiff::Timestamp::from_millisecond((env.clock)()).map_err(|e| Error::Other(e.to_string()))?;
    // 7 days is what the scheduled summary sends (a rolling window); show it as-is.
    let texts = render_window(&db, &env.cfg, &players, days, Some(now.to_zoned(env.cfg.tz().clone())), days == 7)?;
    Ok(texts.iter().map(|t| format!("{}\n\n", strip_tags(t))).collect())
}

/// The live smoke test: the app's own client (no retries) against the real API.
pub fn smoke(cfg: &Config, riot_id: &str, platform: Option<&str>) -> i32 {
    use crate::http::UreqHttp;
    use crate::riot::client::{APP_RATE_LIMITS, HttpRiotClient, RateLimiter};

    let Some((name, tag)) = parse_riot_id(riot_id) else {
        eprintln!("expected GameName#TAG, got {riot_id:?}");
        return 2;
    };
    let platform = platform.unwrap_or(&cfg.default_platform);
    let http = Arc::new(UreqHttp::new(std::time::Duration::from_secs(30)));
    let riot =
        HttpRiotClient::new(&cfg.riot_api_key, http, RateLimiter::new(APP_RATE_LIMITS, Arc::new(RealPacer::default())))
            .with_base_url(crate::app::riot_base_url());
    let mut results: Vec<(&str, bool)> = Vec::new();

    println!("\n=== a) account by Riot ID");
    let acct = match riot.resolve_account(&name, &tag, platform) {
        Ok(acct) => acct,
        Err(e) => {
            println!("FAIL: {}: {e} — cannot continue without a PUUID (403 => key expired/invalid)", e.kind());
            return 1;
        }
    };
    let short: String = acct.puuid.chars().take(12).collect();
    println!("OK  {}#{}  level={:?}  puuid={short}…", acct.game_name, acct.tag_line, acct.summoner_level);
    results.push(("a", true));

    println!("\n=== b) match-v5 history + full match parse");
    let b = riot.recent_match_ids(&acct.puuid, platform, None, 5).and_then(|ids| {
        println!("ids: {ids:?}");
        let first = ids.first().ok_or_else(|| RiotError::NotFound("no recent matches".into()))?;
        riot.get_match(first, platform)
    });
    match b {
        Ok(m) => {
            let me = m.participants.get(&acct.puuid);
            println!("OK  queue={} duration={}s remake={} me={me:?}", m.queue_id, m.duration_s, m.remake);
            results.push(("b", me.is_some()));
        }
        Err(e) => {
            println!("FAIL: {}: {e}", e.kind());
            results.push(("b", false));
        }
    }

    println!("\n=== c) spectator-v5 by PUUID");
    match riot.is_in_game(&acct.puuid, platform) {
        Ok(live) => {
            println!("OK  in_game={live}  (false is the normal answer when idle)");
            results.push(("c", true));
        }
        Err(e) => {
            println!("FAIL: {}: {e}", e.kind());
            results.push(("c", false));
        }
    }

    let summary: HashMap<&str, &str> = results.iter().map(|(k, ok)| (*k, if *ok { "PASS" } else { "FAIL" })).collect();
    let mut keys: Vec<_> = summary.iter().collect();
    keys.sort();
    println!("\nSUMMARY: {keys:?}");
    if results.iter().all(|(_, ok)| *ok) { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::now_ms;
    use crate::models::AccountInfo;
    use crate::testutil::{FakeRiot, MIN, T0, TempDir, TestClock, make_match, make_player_full};

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn command_lines() {
        assert_eq!(parse_args(&[]), Ok(Command::Run));
        assert_eq!(parse_args(&args("backfill")), Ok(Command::Backfill { days: 14 }));
        assert_eq!(parse_args(&args("backfill --days 3")), Ok(Command::Backfill { days: 3 }));
        assert_eq!(parse_args(&args("dry-run-summary")), Ok(Command::DryRunSummary { days: 7 }));
        assert_eq!(
            parse_args(&args("resolve Faker#KR1 --platform KR")),
            Ok(Command::Resolve { riot_id: "Faker#KR1".into(), platform: Some("kr".into()) })
        );
        assert!(parse_args(&args("resolve Faker#KR1 --platform mars")).is_err());
        assert!(parse_args(&args("backfill --days many")).is_err());
        assert!(parse_args(&args("frobnicate")).is_err());
    }

    struct Fixture {
        _dir: TempDir,
        env: CliEnv,
        riot: Arc<FakeRiot>,
    }

    fn fixture(key: &str) -> Fixture {
        let dir = TempDir::new();
        let mut cfg = Config::for_tests(&[1]);
        cfg.riot_api_key = key.into();
        cfg.weekly.tz = jiff::tz::TimeZone::UTC;
        cfg.db_path = dir.path().join("cli.db").to_string_lossy().into_owned();
        let riot = Arc::new(FakeRiot::default());
        let env = CliEnv { cfg, riot: riot.clone(), clock: TestClock::new(now_ms()).as_clock() };
        Fixture { _dir: dir, env, riot }
    }

    fn seed(env: &CliEnv) {
        let db = Database::open(&env.cfg.db_path).unwrap();
        db.add_player(&make_player_full("OLD-PUUID", "Alice", "EUW")).unwrap();
        let mut m = make_match("m1", T0, 30);
        m.puuid = "OLD-PUUID".into();
        db.insert_match(&m).unwrap();
        let sid = db.create_session("OLD-PUUID", T0, T0 + 30 * MIN, T0 + 45 * MIN).unwrap();
        db.attach_match(sid, "m1", "OLD-PUUID").unwrap();
        db.kv_set(KEY_FP, &key_fingerprint("RGAPI-old")).unwrap();
    }

    #[test]
    fn repuuid_rewrites_every_table_and_records_the_new_key() {
        let f = fixture("RGAPI-new");
        seed(&f.env);
        f.riot.set_account(
            "alice",
            "euw",
            AccountInfo {
                puuid: "NEW-PUUID".into(),
                game_name: "Alice".into(),
                tag_line: "EUW".into(),
                summoner_level: Some(30),
            },
        );
        assert_eq!(repuuid(&f.env).unwrap(), 0);

        let db = Database::open(&f.env.cfg.db_path).unwrap();
        assert!(db.get_player("OLD-PUUID").unwrap().is_none());
        assert_eq!(db.get_player("NEW-PUUID").unwrap().unwrap().riot_id(), "Alice#EUW");
        assert_eq!(db.matches_between("NEW-PUUID", 0, 1 << 60).unwrap().len(), 1);
        assert_eq!(db.open_sessions(Some("NEW-PUUID")).unwrap()[0].puuid, "NEW-PUUID");
        assert_eq!(db.kv_get(KEY_FP).unwrap(), Some(key_fingerprint("RGAPI-new"))); // warning silenced
    }

    #[test]
    fn repuuid_leaves_the_warning_armed_if_someone_cannot_be_resolved() {
        let f = fixture("RGAPI-new");
        seed(&f.env);
        assert_eq!(repuuid(&f.env).unwrap(), 1); // FakeRiot: unknown Riot ID -> NotFound
        let db = Database::open(&f.env.cfg.db_path).unwrap();
        assert_eq!(db.kv_get(KEY_FP).unwrap(), Some(key_fingerprint("RGAPI-old"))); // not marked fixed
        assert!(db.get_player("OLD-PUUID").unwrap().is_some());
    }

    #[test]
    fn dry_run_summary_prints_plain_text_and_sends_nothing() {
        let f = fixture("RGAPI-new");
        let db = Database::open(&f.env.cfg.db_path).unwrap();
        db.add_player(&make_player_full("puuid-alice", "Alice", "EUW")).unwrap();
        db.insert_match(&make_match("m1", now_ms() - 60 * MIN, 30)).unwrap();
        let out = dry_run_summary(&f.env, 7).unwrap();
        assert!(out.contains("Week ending") && out.contains("Alice#EUW") && out.contains("1 game"), "{out}");
        assert!(!out.contains("<b>") && !out.contains("<pre>"));
    }

    #[test]
    fn backfill_stores_silently_for_every_player() {
        let f = fixture("RGAPI-new");
        let db = Database::open(&f.env.cfg.db_path).unwrap();
        db.add_player(&make_player_full("puuid-alice", "Alice", "EUW")).unwrap();
        f.riot.set_ids("puuid-alice", &["EUW1_1"]);
        f.riot.add_match(crate::testutil::match_data("EUW1_1", now_ms() - 60 * MIN, 30, &["puuid-alice"]));
        assert_eq!(backfill(&f.env, 14).unwrap(), 0);
        assert_eq!(db.matches_between("puuid-alice", 0, 1 << 60).unwrap().len(), 1);
        assert!(db.open_sessions(None).unwrap().is_empty());
    }
}
