//! SQLite storage: schema, migrations and every query the app runs.
//!
//! Times are UTC epoch milliseconds. One connection is shared behind a mutex: every
//! query is microseconds, so there is nothing to gain from a pool. The schema and
//! the key fingerprint are byte-for-byte those of the Python version, so an existing
//! database is picked up as is.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use sha2::{Digest, Sha256};

use crate::models::{Match, Player, Session};

pub const SCHEMA_VERSION: i64 = 2;

pub const SCHEMA_V1: &str = "
CREATE TABLE players (
  puuid          TEXT PRIMARY KEY,
  game_name      TEXT NOT NULL,
  tag_line       TEXT NOT NULL,
  platform       TEXT NOT NULL,
  active         INTEGER NOT NULL DEFAULT 1,
  added_by       INTEGER,
  added_at       INTEGER NOT NULL,
  last_seen_ms   INTEGER,
  name_checked_at INTEGER
);

CREATE TABLE matches (
  match_id       TEXT NOT NULL,
  puuid          TEXT NOT NULL,
  queue_id       INTEGER NOT NULL,
  champion       TEXT NOT NULL,
  win            INTEGER NOT NULL,        -- 1 win, 0 loss, -1 unknown
  remake         INTEGER NOT NULL DEFAULT 0,
  kills          INTEGER, deaths INTEGER, assists INTEGER,
  duration_s     INTEGER NOT NULL,
  start_ms       INTEGER NOT NULL,
  end_ms         INTEGER NOT NULL,
  ingested_at    INTEGER NOT NULL,
  PRIMARY KEY (match_id, puuid)
);
CREATE INDEX idx_matches_puuid_start ON matches(puuid, start_ms);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  puuid          TEXT NOT NULL,
  started_ms     INTEGER NOT NULL,
  last_end_ms    INTEGER NOT NULL,
  status         TEXT NOT NULL,           -- 'open' | 'flushed'
  flush_due_ms   INTEGER,
  postpones      INTEGER NOT NULL DEFAULT 0,
  notified_at    INTEGER
);
CREATE INDEX idx_sessions_open ON sessions(puuid, status);

CREATE TABLE session_matches (
  session_id     INTEGER NOT NULL,
  match_id       TEXT NOT NULL,
  puuid          TEXT NOT NULL,
  PRIMARY KEY (session_id, match_id, puuid)
);

CREATE TABLE subscribers (
  chat_id        INTEGER PRIMARY KEY,
  added_at       INTEGER NOT NULL
);
";

const SCHEMA_V2: &str = "
CREATE TABLE allowed_users (
  user_id        INTEGER PRIMARY KEY,
  added_by       INTEGER,
  added_at       INTEGER NOT NULL
);
";

/// version -> script that upgrades from version-1. Append; never edit old entries.
const MIGRATIONS: &[(i64, &str)] = &[(1, SCHEMA_V1), (2, SCHEMA_V2)];

pub type DbResult<T> = rusqlite::Result<T>;

/// Stable, non-reversible identifier for an API key (for change detection).
pub fn key_fingerprint(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn player(row: &Row) -> DbResult<Player> {
    Ok(Player {
        puuid: row.get("puuid")?,
        game_name: row.get("game_name")?,
        tag_line: row.get("tag_line")?,
        platform: row.get("platform")?,
        active: row.get::<_, i64>("active")? != 0,
        added_by: row.get("added_by")?,
        added_at: row.get("added_at")?,
        last_seen_ms: row.get("last_seen_ms")?,
        name_checked_at: row.get("name_checked_at")?,
    })
}

fn match_row(row: &Row) -> DbResult<Match> {
    Ok(Match {
        match_id: row.get("match_id")?,
        puuid: row.get("puuid")?,
        queue_id: row.get("queue_id")?,
        champion: row.get("champion")?,
        win: row.get("win")?,
        remake: row.get::<_, i64>("remake")? != 0,
        kills: row.get("kills")?,
        deaths: row.get("deaths")?,
        assists: row.get("assists")?,
        duration_s: row.get("duration_s")?,
        start_ms: row.get("start_ms")?,
        end_ms: row.get("end_ms")?,
        ingested_at: row.get("ingested_at")?,
    })
}

fn session(row: &Row) -> DbResult<Session> {
    Ok(Session {
        id: row.get("id")?,
        puuid: row.get("puuid")?,
        started_ms: row.get("started_ms")?,
        last_end_ms: row.get("last_end_ms")?,
        status: row.get("status")?,
        flush_due_ms: row.get("flush_due_ms")?,
        postpones: row.get("postpones")?,
        notified_at: row.get("notified_at")?,
    })
}

/// Riot IDs are case-insensitive, Unicode-wide (SQLite's NOCASE only folds ASCII).
fn fold(s: &str) -> String {
    s.to_lowercase().replace('ß', "ss")
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        // WAL + NORMAL: crash-safe, and a commit is an append rather than an fsync storm.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite itself consistent.
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn migrate(&self) -> DbResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)", [])?;
        let current: i64 = tx
            .query_row("SELECT v FROM kv WHERE k='schema_version'", [], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if current > SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "database schema v{current} is newer than this code (v{SCHEMA_VERSION})"
            )));
        }
        for &(version, script) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
            tx.execute_batch(script)?;
            tx.execute(
                "INSERT INTO kv(k, v) VALUES('schema_version', ?1) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                [version.to_string()],
            )?;
        }
        tx.commit()
    }

    // -- kv -------------------------------------------------------------------

    pub fn kv_get(&self, key: &str) -> DbResult<Option<String>> {
        self.conn().prepare_cached("SELECT v FROM kv WHERE k=?1")?.query_row([key], |r| r.get(0)).optional()
    }

    pub fn kv_set(&self, key: &str, value: &str) -> DbResult<()> {
        self.conn()
            .prepare_cached("INSERT INTO kv(k, v) VALUES(?1, ?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v")?
            .execute([key, value])
            .map(drop)
    }

    // -- players --------------------------------------------------------------

    /// Insert, or reactivate + refresh names if the PUUID is already known.
    pub fn add_player(&self, p: &Player) -> DbResult<()> {
        self.conn()
            .prepare_cached(
                "INSERT INTO players(puuid, game_name, tag_line, platform, active, added_by, \
                 added_at, last_seen_ms, name_checked_at) VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8) \
                 ON CONFLICT(puuid) DO UPDATE SET active=1, game_name=excluded.game_name, \
                 tag_line=excluded.tag_line, platform=excluded.platform, \
                 name_checked_at=excluded.name_checked_at, \
                 last_seen_ms=CASE WHEN players.active=0 THEN excluded.last_seen_ms \
                 ELSE players.last_seen_ms END",
            )?
            // A player coming back from /remove is new again: games played while they
            // were untracked must not be announced as fresh (the CASE above).
            .execute(params![
                p.puuid,
                p.game_name,
                p.tag_line,
                p.platform,
                p.added_by,
                p.added_at,
                p.last_seen_ms,
                p.name_checked_at
            ])
            .map(drop)
    }

    pub fn get_player(&self, puuid: &str) -> DbResult<Option<Player>> {
        self.conn().prepare_cached("SELECT * FROM players WHERE puuid=?1")?.query_row([puuid], player).optional()
    }

    pub fn find_player(&self, game_name: &str, tag_line: &str, active_only: bool) -> DbResult<Option<Player>> {
        let wanted = (fold(game_name), fold(tag_line));
        Ok(self.list_players(active_only)?.into_iter().find(|p| (fold(&p.game_name), fold(&p.tag_line)) == wanted))
    }

    pub fn list_players(&self, active_only: bool) -> DbResult<Vec<Player>> {
        let sql = if active_only {
            "SELECT * FROM players WHERE active=1 ORDER BY game_name COLLATE NOCASE"
        } else {
            "SELECT * FROM players ORDER BY game_name COLLATE NOCASE"
        };
        self.conn().prepare_cached(sql)?.query_map([], player)?.collect()
    }

    pub fn deactivate_player(&self, puuid: &str) -> DbResult<()> {
        self.conn().prepare_cached("UPDATE players SET active=0 WHERE puuid=?1")?.execute([puuid]).map(drop)
    }

    pub fn set_last_seen(&self, puuid: &str, last_seen_ms: i64) -> DbResult<()> {
        self.conn()
            .prepare_cached(
                "UPDATE players SET last_seen_ms=?1 WHERE puuid=?2 AND (last_seen_ms IS NULL OR last_seen_ms < ?1)",
            )?
            .execute(params![last_seen_ms, puuid])
            .map(drop)
    }

    pub fn update_names(&self, puuid: &str, game_name: &str, tag_line: &str, checked_at: i64) -> DbResult<()> {
        self.conn()
            .prepare_cached("UPDATE players SET game_name=?1, tag_line=?2, name_checked_at=?3 WHERE puuid=?4")?
            .execute(params![game_name, tag_line, checked_at, puuid])
            .map(drop)
    }

    /// Cascade a PUUID change through every table.
    pub fn rewrite_puuid(&self, old: &str, new: &str) -> DbResult<()> {
        if old == new {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for table in ["players", "matches", "sessions", "session_matches"] {
            tx.execute(&format!("UPDATE {table} SET puuid=?1 WHERE puuid=?2"), [new, old])?;
        }
        tx.commit()
    }

    // -- matches --------------------------------------------------------------

    pub fn known_match_ids(&self, puuid: &str, match_ids: &[String]) -> DbResult<HashSet<String>> {
        if match_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let marks = vec!["?"; match_ids.len()].join(",");
        let sql = format!("SELECT match_id FROM matches WHERE puuid=? AND match_id IN ({marks})");
        let args = std::iter::once(puuid).chain(match_ids.iter().map(String::as_str));
        self.conn().prepare_cached(&sql)?.query_map(params_from_iter(args), |r| r.get(0))?.collect()
    }

    /// true if inserted, false if it was already stored (idempotent).
    pub fn insert_match(&self, m: &Match) -> DbResult<bool> {
        let changed = self
            .conn()
            .prepare_cached(
                "INSERT OR IGNORE INTO matches(match_id, puuid, queue_id, champion, win, remake, \
                 kills, deaths, assists, duration_s, start_ms, end_ms, ingested_at) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            )?
            .execute(params![
                m.match_id,
                m.puuid,
                m.queue_id,
                m.champion,
                m.win,
                m.remake as i64,
                m.kills,
                m.deaths,
                m.assists,
                m.duration_s,
                m.start_ms,
                m.end_ms,
                m.ingested_at
            ])?;
        Ok(changed == 1)
    }

    /// Un-store a match so the next poll ingests it again.
    pub fn delete_match(&self, match_id: &str, puuid: &str) -> DbResult<()> {
        self.conn()
            .prepare_cached("DELETE FROM matches WHERE match_id=?1 AND puuid=?2")?
            .execute([match_id, puuid])
            .map(drop)
    }

    /// Matches that *started* in [start_ms, end_ms), oldest first.
    pub fn matches_between(&self, puuid: &str, start_ms: i64, end_ms: i64) -> DbResult<Vec<Match>> {
        self.conn()
            .prepare_cached("SELECT * FROM matches WHERE puuid=?1 AND start_ms>=?2 AND start_ms<?3 ORDER BY start_ms")?
            .query_map(params![puuid, start_ms, end_ms], match_row)?
            .collect()
    }

    /// puuid -> end time of the player's most recent stored game.
    pub fn last_game_ends(&self) -> DbResult<HashMap<String, i64>> {
        self.conn()
            .prepare_cached("SELECT puuid, MAX(end_ms) FROM matches GROUP BY puuid")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    pub fn session_matches(&self, session_id: i64) -> DbResult<Vec<Match>> {
        self.conn()
            .prepare_cached(
                "SELECT m.* FROM matches m JOIN session_matches sm \
                 ON sm.match_id=m.match_id AND sm.puuid=m.puuid \
                 WHERE sm.session_id=?1 ORDER BY m.start_ms",
            )?
            .query_map([session_id], match_row)?
            .collect()
    }

    // -- sessions -------------------------------------------------------------

    pub fn get_session(&self, session_id: i64) -> DbResult<Option<Session>> {
        self.conn().prepare_cached("SELECT * FROM sessions WHERE id=?1")?.query_row([session_id], session).optional()
    }

    pub fn open_sessions(&self, puuid: Option<&str>) -> DbResult<Vec<Session>> {
        let conn = self.conn();
        match puuid {
            None => conn
                .prepare_cached("SELECT * FROM sessions WHERE status='open' ORDER BY id")?
                .query_map([], session)?
                .collect(),
            Some(puuid) => conn
                .prepare_cached(
                    "SELECT * FROM sessions WHERE status='open' AND puuid=?1 ORDER BY last_end_ms DESC, id DESC",
                )?
                .query_map([puuid], session)?
                .collect(),
        }
    }

    pub fn create_session(&self, puuid: &str, started_ms: i64, last_end_ms: i64, flush_due_ms: i64) -> DbResult<i64> {
        let conn = self.conn();
        conn.prepare_cached(
            "INSERT INTO sessions(puuid, started_ms, last_end_ms, status, flush_due_ms) VALUES(?1,?2,?3,'open',?4)",
        )?
        .execute(params![puuid, started_ms, last_end_ms, flush_due_ms])?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_session(
        &self,
        session_id: i64,
        started_ms: i64,
        last_end_ms: i64,
        flush_due_ms: Option<i64>,
        postpones: i64,
    ) -> DbResult<()> {
        self.conn()
            .prepare_cached(
                "UPDATE sessions SET started_ms=?1, last_end_ms=?2, flush_due_ms=?3, postpones=?4 WHERE id=?5",
            )?
            .execute(params![started_ms, last_end_ms, flush_due_ms, postpones, session_id])
            .map(drop)
    }

    pub fn set_postpone(&self, session_id: i64, postpones: i64, flush_due_ms: i64) -> DbResult<()> {
        self.conn()
            .prepare_cached("UPDATE sessions SET postpones=?1, flush_due_ms=?2 WHERE id=?3")?
            .execute(params![postpones, flush_due_ms, session_id])
            .map(drop)
    }

    pub fn attach_match(&self, session_id: i64, match_id: &str, puuid: &str) -> DbResult<()> {
        self.conn()
            .prepare_cached("INSERT OR IGNORE INTO session_matches(session_id, match_id, puuid) VALUES(?1,?2,?3)")?
            .execute(params![session_id, match_id, puuid])
            .map(drop)
    }

    pub fn mark_flushed(&self, session_id: i64, notified_at: i64) -> DbResult<()> {
        self.conn()
            .prepare_cached("UPDATE sessions SET status='flushed', notified_at=?1, flush_due_ms=NULL WHERE id=?2")?
            .execute(params![notified_at, session_id])
            .map(drop)
    }

    // -- subscribers ----------------------------------------------------------

    pub fn add_subscriber(&self, chat_id: i64, added_at: i64) -> DbResult<bool> {
        let n = self
            .conn()
            .prepare_cached("INSERT OR IGNORE INTO subscribers(chat_id, added_at) VALUES(?1,?2)")?
            .execute([chat_id, added_at])?;
        Ok(n == 1)
    }

    pub fn remove_subscriber(&self, chat_id: i64) -> DbResult<bool> {
        let n = self.conn().prepare_cached("DELETE FROM subscribers WHERE chat_id=?1")?.execute([chat_id])?;
        Ok(n == 1)
    }

    pub fn is_subscriber(&self, chat_id: i64) -> DbResult<bool> {
        self.conn().prepare_cached("SELECT 1 FROM subscribers WHERE chat_id=?1")?.exists([chat_id])
    }

    pub fn subscribers(&self) -> DbResult<Vec<i64>> {
        self.conn()
            .prepare_cached("SELECT chat_id FROM subscribers ORDER BY chat_id")?
            .query_map([], |r| r.get(0))?
            .collect()
    }

    // -- allowed users (who may talk to the bot at all) ------------------------

    pub fn add_allowed_user(&self, user_id: i64, added_by: Option<i64>, added_at: i64) -> DbResult<bool> {
        let n = self
            .conn()
            .prepare_cached("INSERT OR IGNORE INTO allowed_users(user_id, added_by, added_at) VALUES(?1,?2,?3)")?
            .execute(params![user_id, added_by, added_at])?;
        Ok(n == 1)
    }

    pub fn remove_allowed_user(&self, user_id: i64) -> DbResult<bool> {
        let n = self.conn().prepare_cached("DELETE FROM allowed_users WHERE user_id=?1")?.execute([user_id])?;
        Ok(n == 1)
    }

    pub fn is_allowed_user(&self, user_id: i64) -> DbResult<bool> {
        self.conn().prepare_cached("SELECT 1 FROM allowed_users WHERE user_id=?1")?.exists([user_id])
    }

    pub fn allowed_users(&self) -> DbResult<Vec<i64>> {
        self.conn()
            .prepare_cached("SELECT user_id FROM allowed_users ORDER BY user_id")?
            .query_map([], |r| r.get(0))?
            .collect()
    }

    #[cfg(test)]
    pub fn break_sessions_table_for_tests(&self) {
        self.conn().execute_batch("ALTER TABLE sessions RENAME TO sessions_broken").unwrap();
    }

    #[cfg(test)]
    pub fn repair_sessions_table_for_tests(&self) {
        let conn = self.conn();
        conn.flush_prepared_statement_cache();
        conn.execute_batch("ALTER TABLE sessions_broken RENAME TO sessions").unwrap();
    }

    #[cfg(test)]
    pub fn break_allowed_users_for_tests(&self) {
        self.conn().execute_batch("DROP TABLE allowed_users").unwrap();
    }

    #[cfg(test)]
    pub fn break_players_table_for_tests(&self) {
        self.conn().execute_batch("ALTER TABLE players RENAME TO players_broken").unwrap();
    }

    #[cfg(test)]
    pub fn table_names(&self) -> Vec<String> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MIN, T0, TempDir, make_match, make_player, make_player_full, test_db};

    const FOREVER: i64 = 1 << 60;

    #[test]
    fn schema_is_created_and_versioned() {
        let db = test_db();
        assert_eq!(db.kv_get("schema_version").unwrap(), Some(SCHEMA_VERSION.to_string()));
        let tables = db.table_names();
        for t in ["players", "matches", "sessions", "session_matches", "subscribers", "kv", "allowed_users"] {
            assert!(tables.iter().any(|n| n == t), "{t}");
        }
    }

    #[test]
    fn reopening_an_existing_database_is_a_noop_migration() {
        let dir = TempDir::new();
        let path = dir.path().join("x.db");
        Database::open(&path).unwrap().add_player(&make_player()).unwrap();
        let second = Database::open(&path).unwrap();
        let ids: Vec<String> = second.list_players(true).unwrap().iter().map(Player::riot_id).collect();
        assert_eq!(ids, ["Alice#EUW"]);
    }

    #[test]
    fn match_insert_is_idempotent() {
        let db = test_db();
        db.add_player(&make_player()).unwrap();
        let m = make_match("EUW1_1", T0, 30);
        assert!(db.insert_match(&m).unwrap());
        assert!(!db.insert_match(&m).unwrap());
        let known = db.known_match_ids("puuid-alice", &["EUW1_1".into(), "EUW1_2".into()]).unwrap();
        assert_eq!(known, HashSet::from(["EUW1_1".to_string()]));
        assert_eq!(db.matches_between("puuid-alice", T0, T0 + 1).unwrap().len(), 1);
    }

    #[test]
    fn matches_between_is_half_open_and_ordered() {
        let db = test_db();
        for (name, offset) in [("c", 30), ("a", 0), ("b", 10)] {
            db.insert_match(&make_match(name, T0 + offset * MIN, 30)).unwrap();
        }
        let got: Vec<String> =
            db.matches_between("puuid-alice", T0, T0 + 30 * MIN).unwrap().into_iter().map(|m| m.match_id).collect();
        assert_eq!(got, ["a", "b"]); // end is exclusive, oldest first
    }

    #[test]
    fn last_seen_only_moves_forward() {
        let db = test_db();
        db.add_player(&make_player()).unwrap();
        db.set_last_seen("puuid-alice", T0 + 10).unwrap();
        db.set_last_seen("puuid-alice", T0).unwrap();
        assert_eq!(db.get_player("puuid-alice").unwrap().unwrap().last_seen_ms, Some(T0 + 10));
    }

    #[test]
    fn soft_delete_keeps_history_and_re_adding_reactivates() {
        let db = test_db();
        let mut alice = make_player();
        alice.last_seen_ms = Some(T0);
        db.add_player(&alice).unwrap();
        db.insert_match(&make_match("m", T0, 30)).unwrap();
        db.deactivate_player("puuid-alice").unwrap();
        assert!(db.list_players(true).unwrap().is_empty());
        assert!(db.find_player("alice", "euw", true).unwrap().is_none());
        assert!(!db.find_player("alice", "euw", false).unwrap().unwrap().active);
        assert_eq!(db.matches_between("puuid-alice", 0, FOREVER).unwrap().len(), 1);

        db.add_player(&make_player_full("puuid-alice", "AliceRenamed", "EUW")).unwrap(); // /add again
        let p = db.get_player("puuid-alice").unwrap().unwrap();
        assert!(p.active && p.game_name == "AliceRenamed");
        // Back from /remove: games played while untracked must not be announced.
        assert_eq!(p.last_seen_ms, None);
    }

    #[test]
    fn re_adding_an_active_player_keeps_the_polling_marker() {
        let db = test_db();
        let mut alice = make_player();
        alice.last_seen_ms = Some(T0);
        db.add_player(&alice).unwrap();
        db.add_player(&make_player_full("puuid-alice", "AliceRenamed", "EUW")).unwrap();
        assert_eq!(db.get_player("puuid-alice").unwrap().unwrap().last_seen_ms, Some(T0));
    }

    #[test]
    fn find_player_is_case_insensitive_beyond_ascii() {
        let db = test_db();
        db.add_player(&make_player()).unwrap();
        db.add_player(&make_player_full("p-ru", "Павел", "РУС")).unwrap();
        assert_eq!(db.find_player("ALICE", "euw", true).unwrap().unwrap().puuid, "puuid-alice");
        assert_eq!(db.find_player("павел", "рус", true).unwrap().unwrap().puuid, "p-ru");
    }

    #[test]
    fn rewrite_puuid_cascades_everywhere() {
        let db = test_db();
        db.add_player(&make_player()).unwrap();
        db.insert_match(&make_match("m1", T0, 30)).unwrap();
        let sid = db.create_session("puuid-alice", T0, T0 + 30 * MIN, T0 + 45 * MIN).unwrap();
        db.attach_match(sid, "m1", "puuid-alice").unwrap();

        db.rewrite_puuid("puuid-alice", "puuid-NEW").unwrap();

        assert!(db.get_player("puuid-alice").unwrap().is_none());
        assert_eq!(db.get_player("puuid-NEW").unwrap().unwrap().riot_id(), "Alice#EUW");
        assert_eq!(db.matches_between("puuid-NEW", 0, FOREVER).unwrap().len(), 1);
        assert_eq!(db.get_session(sid).unwrap().unwrap().puuid, "puuid-NEW");
        assert_eq!(db.session_matches(sid).unwrap()[0].match_id, "m1"); // join still works
    }

    #[test]
    fn session_lifecycle() {
        let db = test_db();
        let sid = db.create_session("puuid-alice", T0, T0 + 30 * MIN, T0 + 45 * MIN).unwrap();
        assert_eq!(db.open_sessions(Some("puuid-alice")).unwrap()[0].id, sid);
        db.set_postpone(sid, 2, T0 + 50 * MIN).unwrap();
        let s = db.get_session(sid).unwrap().unwrap();
        assert_eq!((s.postpones, s.flush_due_ms), (2, Some(T0 + 50 * MIN)));
        db.mark_flushed(sid, T0 + 60 * MIN).unwrap();
        let s = db.get_session(sid).unwrap().unwrap();
        assert_eq!((s.status.as_str(), s.notified_at, s.flush_due_ms), ("flushed", Some(T0 + 60 * MIN), None));
        assert!(db.open_sessions(None).unwrap().is_empty());
    }

    #[test]
    fn subscribers_and_kv() {
        let db = test_db();
        assert!(db.add_subscriber(-100, T0).unwrap());
        assert!(!db.add_subscriber(-100, T0).unwrap());
        assert!(db.is_subscriber(-100).unwrap() && !db.is_subscriber(5).unwrap());
        assert_eq!(db.subscribers().unwrap(), [-100]);
        assert!(db.remove_subscriber(-100).unwrap());
        assert!(!db.remove_subscriber(-100).unwrap());

        db.kv_set("k", "1").unwrap();
        db.kv_set("k", "2").unwrap();
        assert_eq!(db.kv_get("k").unwrap().as_deref(), Some("2"));
        assert_eq!(db.kv_get("missing").unwrap(), None);
    }

    #[test]
    fn allowed_users() {
        let db = test_db();
        assert!(db.add_allowed_user(7, Some(1), T0).unwrap());
        assert!(!db.add_allowed_user(7, Some(1), T0).unwrap());
        assert!(db.add_allowed_user(3, None, T0).unwrap());
        assert!(db.is_allowed_user(7).unwrap() && !db.is_allowed_user(8).unwrap());
        assert_eq!(db.allowed_users().unwrap(), [3, 7]);
        assert!(db.remove_allowed_user(7).unwrap());
        assert!(!db.remove_allowed_user(7).unwrap());
        assert!(!db.is_allowed_user(7).unwrap());
    }

    #[test]
    fn delete_match_unstores_it() {
        let db = test_db();
        db.insert_match(&make_match("m1", T0, 30)).unwrap();
        db.delete_match("m1", "puuid-alice").unwrap();
        assert!(db.known_match_ids("puuid-alice", &["m1".into()]).unwrap().is_empty());
    }

    #[test]
    fn key_fingerprint_matches_the_python_version_and_hides_the_key() {
        // hashlib.sha256(b"RGAPI-secret").hexdigest()[:16]
        assert_eq!(key_fingerprint("RGAPI-secret"), "cb75621ab4ee3e49");
        assert_eq!(key_fingerprint("abc"), "ba7816bf8f01cfea"); // the SHA-256 test vector
        assert_eq!(key_fingerprint("a").len(), 16);
        assert_ne!(key_fingerprint("a"), key_fingerprint("b"));
        assert!(!key_fingerprint("RGAPI-secret").contains("secret"));
    }

    #[test]
    fn a_version_1_database_upgrades_and_keeps_its_data() {
        let dir = TempDir::new();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);").unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch("INSERT INTO kv VALUES('schema_version', '1'); INSERT INTO subscribers VALUES(-5, 0);")
                .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.kv_get("schema_version").unwrap(), Some(SCHEMA_VERSION.to_string()));
        assert_eq!(db.subscribers().unwrap(), [-5]);
        assert!(db.allowed_users().unwrap().is_empty());
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = TempDir::new();
        let path = dir.path().join("new.db");
        Database::open(&path).unwrap().kv_set("schema_version", "99").unwrap();
        assert!(Database::open(&path).is_err());
    }
}
