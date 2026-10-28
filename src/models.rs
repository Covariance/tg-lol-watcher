//! Plain data shared across modules, and the queue table.

use std::collections::HashMap;

/// `Match::win` is stored as an INTEGER: 1 win, 0 loss, -1 unknown (Arena, odd modes).
pub const WIN: i64 = 1;
pub const LOSS: i64 = 0;
pub const UNKNOWN: i64 = -1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub puuid: String,
    pub game_name: String,
    pub tag_line: String,
    pub summoner_level: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantResult {
    pub champion: String,
    pub win: i64,
    pub kills: Option<i64>,
    pub deaths: Option<i64>,
    pub assists: Option<i64>,
}

/// A parsed match-v5 game. `participants` is keyed by PUUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchData {
    pub match_id: String,
    pub queue_id: i64,
    pub duration_s: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub remake: bool,
    pub participants: HashMap<String, ParticipantResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub puuid: String,
    pub game_name: String,
    pub tag_line: String,
    pub platform: String,
    pub active: bool,
    pub added_by: Option<i64>,
    pub added_at: i64,
    pub last_seen_ms: Option<i64>,
    pub name_checked_at: Option<i64>,
}

impl Player {
    pub fn riot_id(&self) -> String {
        format!("{}#{}", self.game_name, self.tag_line)
    }
}

/// One tracked player's row in `matches`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub match_id: String,
    pub puuid: String,
    pub queue_id: i64,
    pub champion: String,
    pub win: i64,
    pub remake: bool,
    pub kills: Option<i64>,
    pub deaths: Option<i64>,
    pub assists: Option<i64>,
    pub duration_s: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub ingested_at: i64,
}

impl Match {
    pub fn is_win(&self) -> bool {
        !self.remake && self.win == WIN
    }

    pub fn is_loss(&self) -> bool {
        !self.remake && self.win == LOSS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub puuid: String,
    pub started_ms: i64,
    pub last_end_ms: i64,
    /// 'open' | 'flushed'
    pub status: String,
    pub flush_due_ms: Option<i64>,
    pub postpones: i64,
    pub notified_at: Option<i64>,
}

impl Session {
    pub fn is_open(&self) -> bool {
        self.status == "open"
    }
}

// -- queues -------------------------------------------------------------------

/// queue_id -> display group, in the order groups appear in summaries.
const GROUPS: &[(&str, &[i64])] = &[
    ("Ranked Solo", &[420]),
    ("Ranked Flex", &[440]),
    ("Normal", &[400, 430, 490]),
    ("ARAM", &[450]),
    ("Arena", &[1700, 1710]),
    ("Bots", &[830, 840, 850, 870, 880, 890]),
    ("Clash", &[700]),
];

/// Queues where `win` has no Summoner's Rift meaning (placements, not win/loss).
pub fn is_no_win_loss_queue(queue_id: i64) -> bool {
    matches!(queue_id, 1700 | 1710)
}

/// Display group for a queue; unknown ids render as `Queue {id}`.
pub fn queue_group(queue_id: i64) -> String {
    GROUPS
        .iter()
        .find(|(_, ids)| ids.contains(&queue_id))
        .map(|(name, _)| (*name).to_owned())
        .unwrap_or_else(|| format!("Queue {queue_id}"))
}

/// Known groups in table order, anything else after them alphabetically.
pub fn group_sort_key(group: &str) -> (usize, &str) {
    let index = GROUPS.iter().position(|(name, _)| *name == group);
    (index.unwrap_or(GROUPS.len()), group)
}
