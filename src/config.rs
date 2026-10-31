//! TOML + environment config, validated as a whole at startup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use toml::{Table, Value};

use crate::riot::is_platform;

pub const MODES: &[&str] = &["session", "per_game", "off"];
pub const DAYS: &[(&str, Weekday)] = &[
    ("sunday", Weekday::Sunday),
    ("monday", Weekday::Monday),
    ("tuesday", Weekday::Tuesday),
    ("wednesday", Weekday::Wednesday),
    ("thursday", Weekday::Thursday),
    ("friday", Weekday::Friday),
    ("saturday", Weekday::Saturday),
];
pub const LOG_LEVELS: &[&str] = &["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Session,
    PerGame,
    Off,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::PerGame => "per_game",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NotificationConfig {
    pub mode: Mode,
    pub session_gap_minutes: i64,
    pub flush_recheck_minutes: i64,
    pub max_postpones: i64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { mode: Mode::Session, session_gap_minutes: 15, flush_recheck_minutes: 5, max_postpones: 18 }
    }
}

impl NotificationConfig {
    pub fn gap_ms(&self) -> i64 {
        self.session_gap_minutes * 60_000
    }

    pub fn recheck_ms(&self) -> i64 {
        self.flush_recheck_minutes * 60_000
    }
}

#[derive(Debug, Clone)]
pub struct WeeklyConfig {
    pub enabled: bool,
    pub day: String,
    pub hour: i8,
    pub minute: i8,
    pub timezone: String,
    pub tz: TimeZone,
}

impl WeeklyConfig {
    pub fn weekday(&self) -> Weekday {
        DAYS.iter().find(|(d, _)| *d == self.day).map(|(_, w)| *w).unwrap_or(Weekday::Sunday)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub default_platform: String,
    pub poll_interval_seconds: i64,
    pub notifications: NotificationConfig,
    pub weekly: WeeklyConfig,
    pub admin_user_ids: Vec<i64>,
    pub log_level: String,
    pub riot_api_key: String,
    pub telegram_bot_token: String,
    pub db_path: String,
}

impl Config {
    /// Timezone used for rendering. Same as the weekly schedule's.
    pub fn tz(&self) -> &TimeZone {
        &self.weekly.tz
    }

    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_ids.contains(&user_id)
    }

    /// Defaults with the given secrets; what tests start from.
    #[cfg(test)]
    pub fn for_tests(admins: &[i64]) -> Self {
        Self {
            default_platform: "euw1".into(),
            poll_interval_seconds: 120,
            notifications: NotificationConfig::default(),
            weekly: WeeklyConfig {
                enabled: true,
                day: "sunday".into(),
                hour: 20,
                minute: 0,
                timezone: "Europe/London".into(),
                tz: TimeZone::get("Europe/London").expect("tz database present"),
            },
            admin_user_ids: admins.to_vec(),
            log_level: "INFO".into(),
            riot_api_key: "k".into(),
            telegram_bot_token: "1:x".into(),
            db_path: "./lolwatch.db".into(),
        }
    }
}

/// Every problem found, one per line.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `GameName#TAG` -> (game_name, tag_line), split on the last '#'.
pub fn parse_riot_id(text: &str) -> Option<(String, String)> {
    let (name, tag) = text.trim().rsplit_once('#')?;
    let (name, tag) = (name.trim(), tag.trim());
    (!name.is_empty() && !tag.is_empty()).then(|| (name.to_owned(), tag.to_owned()))
}

/// Python-ish repr for error messages: strings quoted, the rest in TOML syntax.
fn repr(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        other => as_text(other),
    }
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Integer(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Boolean(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Datetime(d) => d.to_string(),
        Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Table(_) => "{...}".into(),
    }
}

fn int(errors: &mut Vec<String>, where_: &str, value: Option<&Value>, default: i64, minimum: i64) -> i64 {
    match value {
        None => default,
        Some(Value::Integer(n)) if *n >= minimum => *n,
        Some(other) => {
            errors.push(format!("{where_}: expected an integer >= {minimum}, got {}", repr(other)));
            minimum
        }
    }
}

fn section<'a>(errors: &mut Vec<String>, raw: &'a Table, name: &str) -> Option<&'a Table> {
    match raw.get(name) {
        None => None,
        Some(Value::Table(t)) => Some(t),
        Some(_) => {
            errors.push(format!("[{name}] must be a table"));
            None
        }
    }
}

/// Minimal `.env` support: `KEY=value` lines, `#` comments, optional `export ` and
/// surrounding quotes. Variables already in `env` win.
pub fn load_dotenv(path: &Path, env: &mut HashMap<String, String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.strip_prefix("export ").unwrap_or(line).split_once('=') else {
            continue;
        };
        let mut value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() >= 2 && bytes[0] == bytes[bytes.len() - 1] && matches!(bytes[0], b'\'' | b'"') {
            value = &value[1..value.len() - 1];
        }
        env.entry(key.trim().to_owned()).or_insert_with(|| value.to_owned());
    }
}

/// The process environment with `./.env` filled in underneath it.
pub fn process_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    load_dotenv(Path::new(".env"), &mut env);
    env
}

/// Load and validate. Fails with *all* problems at once.
///
/// `need_telegram = false` (CLI subcommands) skips the Telegram token and admin list.
pub fn load_config(
    path: Option<&Path>,
    env: &HashMap<String, String>,
    need_telegram: bool,
) -> Result<Config, ConfigError> {
    let get = |k: &str| env.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let path: PathBuf = path
        .map(Path::to_path_buf)
        .or_else(|| get("LOLWATCH_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| "config.toml".into());
    let mut errors: Vec<String> = Vec::new();

    let raw: Table = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            errors.push(format!("config file not found: {} (copy config.example.toml)", path.display()));
            Table::new()
        }
        Err(e) => {
            errors.push(format!("config file {} could not be read: {e}", path.display()));
            Table::new()
        }
        Ok(text) => text.parse::<Table>().unwrap_or_else(|e| {
            errors.push(format!("config file {} is not valid TOML: {}", path.display(), e.to_string().trim()));
            Table::new()
        }),
    };
    let empty = Table::new();

    let riot = section(&mut errors, &raw, "riot").unwrap_or(&empty);
    let default_platform = riot.get("default_platform").map(as_text).unwrap_or_else(|| "euw1".into()).to_lowercase();
    if !is_platform(&default_platform) {
        errors.push(format!("riot.default_platform: unknown platform '{default_platform}'"));
    }
    let poll = int(&mut errors, "riot.poll_interval_seconds", riot.get("poll_interval_seconds"), 120, 10);

    let n = section(&mut errors, &raw, "notifications").unwrap_or(&empty);
    let mode = match n.get("mode") {
        None => Mode::Session,
        Some(Value::String(s)) if s == "session" => Mode::Session,
        Some(Value::String(s)) if s == "per_game" => Mode::PerGame,
        Some(Value::String(s)) if s == "off" => Mode::Off,
        Some(other) => {
            errors.push(format!("notifications.mode: expected one of {MODES:?}, got {}", repr(other)));
            Mode::Session
        }
    };
    let notifications = NotificationConfig {
        mode,
        session_gap_minutes: int(&mut errors, "notifications.session_gap_minutes", n.get("session_gap_minutes"), 15, 1),
        flush_recheck_minutes: int(
            &mut errors,
            "notifications.flush_recheck_minutes",
            n.get("flush_recheck_minutes"),
            5,
            1,
        ),
        max_postpones: int(&mut errors, "notifications.max_postpones", n.get("max_postpones"), 18, 0),
    };

    let w = section(&mut errors, &raw, "weekly").unwrap_or(&empty);
    let mut day = w.get("day").map(as_text).unwrap_or_else(|| "sunday".into()).to_lowercase();
    if !DAYS.iter().any(|(d, _)| *d == day) {
        errors.push(format!("weekly.day: expected a weekday name, got '{day}'"));
        day = "sunday".into();
    }
    let time_text = w.get("time").map(as_text).unwrap_or_else(|| "20:00".into());
    let (hour, minute) = parse_hh_mm(&time_text).unwrap_or_else(|| {
        errors.push(format!("weekly.time: expected HH:MM, got '{time_text}'"));
        (20, 0)
    });
    let timezone = w.get("timezone").map(as_text).unwrap_or_else(|| "Europe/London".into());
    let tz = TimeZone::get(&timezone).unwrap_or_else(|_| {
        errors.push(format!("weekly.timezone: unknown timezone '{timezone}'"));
        TimeZone::UTC
    });
    let enabled = match w.get("enabled") {
        None => true,
        Some(Value::Boolean(b)) => *b,
        Some(other) => {
            errors.push(format!("weekly.enabled: expected true/false, got {}", repr(other)));
            true
        }
    };

    let t = section(&mut errors, &raw, "telegram").unwrap_or(&empty);
    let admins: Option<Vec<i64>> = match t.get("admin_user_ids") {
        None => Some(vec![]),
        Some(Value::Array(items)) => items.iter().map(|v| v.as_integer()).collect(),
        Some(_) => None,
    };
    let admin_user_ids = match admins {
        Some(ids) if !(need_telegram && ids.is_empty()) => ids,
        _ => {
            errors.push("telegram.admin_user_ids: expected a non-empty list of integer user ids".into());
            vec![]
        }
    };

    let logging = section(&mut errors, &raw, "logging").unwrap_or(&empty);
    let mut level = logging.get("level").map(as_text).unwrap_or_else(|| "INFO".into()).to_uppercase();
    if !LOG_LEVELS.contains(&level.as_str()) {
        errors.push(format!("logging.level: expected one of {LOG_LEVELS:?}, got '{level}'"));
        level = "INFO".into();
    }

    let riot_key = env.get("RIOT_API_KEY").map(|v| v.trim().to_owned()).unwrap_or_default();
    let tg_token = env.get("TELEGRAM_BOT_TOKEN").map(|v| v.trim().to_owned()).unwrap_or_default();
    if riot_key.is_empty() {
        errors.push("environment: RIOT_API_KEY is not set".into());
    }
    if need_telegram && tg_token.is_empty() {
        errors.push("environment: TELEGRAM_BOT_TOKEN is not set".into());
    }

    if !errors.is_empty() {
        return Err(ConfigError(errors.join("\n")));
    }
    Ok(Config {
        default_platform,
        poll_interval_seconds: poll,
        notifications,
        weekly: WeeklyConfig { enabled, day, hour, minute, timezone, tz },
        admin_user_ids,
        log_level: level,
        riot_api_key: riot_key,
        telegram_bot_token: tg_token,
        db_path: get("LOLWATCH_DB").unwrap_or("./lolwatch.db").to_owned(),
    })
}

fn parse_hh_mm(text: &str) -> Option<(i8, i8)> {
    let (h, m) = text.split_once(':')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(h) || h.len() > 2 || !digits(m) || m.len() != 2 {
        return None;
    }
    let (h, m): (i8, i8) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const VALID: &str = r#"
[riot]
default_platform = "kr"
poll_interval_seconds = 60

[notifications]
mode = "per_game"

[weekly]
day = "Friday"
time = "21:30"
timezone = "Europe/London"

[telegram]
admin_user_ids = [1, 2]
"#;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    fn secrets() -> HashMap<String, String> {
        env(&[("RIOT_API_KEY", "RGAPI-x"), ("TELEGRAM_BOT_TOKEN", "123:abc")])
    }

    fn write(dir: &TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn valid_config_loads_with_env_secrets() {
        let dir = TempDir::new();
        let cfg = load_config(Some(&write(&dir, VALID)), &secrets(), true).unwrap();
        assert_eq!((cfg.default_platform.as_str(), cfg.poll_interval_seconds), ("kr", 60));
        assert_eq!(cfg.notifications.mode, Mode::PerGame);
        assert_eq!(cfg.notifications.gap_ms(), 15 * 60_000); // defaults fill the gaps
        assert_eq!((cfg.weekly.weekday(), cfg.weekly.hour, cfg.weekly.minute), (Weekday::Friday, 21, 30));
        assert_eq!(cfg.admin_user_ids, [1, 2]);
        assert_eq!((cfg.riot_api_key.as_str(), cfg.telegram_bot_token.as_str()), ("RGAPI-x", "123:abc"));
        assert_eq!(cfg.db_path, "./lolwatch.db");
    }

    #[test]
    fn db_path_comes_from_env_and_empty_falls_back() {
        let dir = TempDir::new();
        let path = write(&dir, VALID);
        let mut e = secrets();
        e.insert("LOLWATCH_DB".into(), "/data/x.db".into());
        assert_eq!(load_config(Some(&path), &e, true).unwrap().db_path, "/data/x.db");
        e.insert("LOLWATCH_DB".into(), String::new());
        assert_eq!(load_config(Some(&path), &e, true).unwrap().db_path, "./lolwatch.db");
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let dir = TempDir::new();
        let bad = r#"
[riot]
default_platform = "mars1"
poll_interval_seconds = 1
[notifications]
mode = "loud"
[weekly]
day = "someday"
time = "25:99"
timezone = "Mars/Olympus"
[telegram]
admin_user_ids = []
[logging]
level = "CHATTY"
"#;
        let text = load_config(Some(&write(&dir, bad)), &HashMap::new(), true).unwrap_err().0;
        for fragment in [
            "default_platform",
            "poll_interval_seconds",
            "notifications.mode",
            "weekly.day",
            "weekly.time",
            "weekly.timezone",
            "admin_user_ids",
            "logging.level",
            "RIOT_API_KEY",
            "TELEGRAM_BOT_TOKEN",
        ] {
            assert!(text.contains(fragment), "{fragment}: {text}");
        }
    }

    #[test]
    fn missing_file_and_invalid_toml_are_clear_errors() {
        let dir = TempDir::new();
        let missing = load_config(Some(&dir.path().join("nope.toml")), &secrets(), true).unwrap_err().0;
        assert!(missing.contains("not found"));
        let invalid = load_config(Some(&write(&dir, "[riot\n")), &secrets(), true).unwrap_err().0;
        assert!(invalid.contains("not valid TOML"));
    }

    #[test]
    fn secrets_in_the_toml_file_are_ignored() {
        let dir = TempDir::new();
        let text = format!("{VALID}\n[secrets]\nRIOT_API_KEY = \"leaked\"\n");
        let err = load_config(Some(&write(&dir, &text)), &env(&[("TELEGRAM_BOT_TOKEN", "x")]), true).unwrap_err();
        assert!(err.0.contains("RIOT_API_KEY"));
    }

    #[test]
    fn cli_mode_does_not_need_telegram() {
        let dir = TempDir::new();
        let path = write(&dir, &VALID.replace("admin_user_ids = [1, 2]", "admin_user_ids = []"));
        let e = env(&[("RIOT_API_KEY", "k")]);
        assert_eq!(load_config(Some(&path), &e, false).unwrap().riot_api_key, "k");
        assert!(load_config(Some(&path), &e, true).is_err());
    }

    #[test]
    fn utc_is_a_valid_timezone() {
        let dir = TempDir::new();
        let path = write(&dir, &VALID.replace("Europe/London", "UTC"));
        assert!(load_config(Some(&path), &secrets(), true).is_ok());
    }

    #[test]
    fn riot_ids() {
        assert_eq!(parse_riot_id("Faker#KR1"), Some(("Faker".into(), "KR1".into())));
        assert_eq!(parse_riot_id("  Two Words #EUW "), Some(("Two Words".into(), "EUW".into())));
        assert_eq!(parse_riot_id("Has#Hash#TAG"), Some(("Has#Hash".into(), "TAG".into())));
        for bad in ["", "nohash", "#TAG", "Name#"] {
            assert_eq!(parse_riot_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn dotenv_parsing_and_precedence() {
        let dir = TempDir::new();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# comment\n\nexport A_KEY=plain\nB_KEY = 'single quoted'\nC_KEY=\"double # not a comment\"\nnot a line\nD_KEY=\nREAL=from-file\n",
        )
        .unwrap();
        let mut e = env(&[("REAL", "from-env")]);
        load_dotenv(&path, &mut e);
        assert_eq!(e["A_KEY"], "plain");
        assert_eq!(e["B_KEY"], "single quoted");
        assert_eq!(e["C_KEY"], "double # not a comment");
        assert_eq!(e["D_KEY"], "");
        assert_eq!(e["REAL"], "from-env"); // the real environment wins
        load_dotenv(&dir.path().join("missing.env"), &mut e); // an absent file is fine
    }

    #[test]
    fn hh_mm() {
        assert_eq!(parse_hh_mm("20:00"), Some((20, 0)));
        assert_eq!(parse_hh_mm("7:05"), Some((7, 5)));
        for bad in ["25:99", "24:00", "20:0", "aa:bb", "123:00", "20"] {
            assert_eq!(parse_hh_mm(bad), None, "{bad}");
        }
    }
}
