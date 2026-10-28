//! The ONLY Riot surface the rest of the app sees: a trait, typed errors and the
//! platform routing table. `client.rs` implements it over HTTP; `retry.rs` wraps any
//! implementation with the retry policy.

pub mod client;
pub mod parsing;
pub mod retry;

use std::fmt;

use crate::models::{AccountInfo, MatchData};

/// Every failure the Riot layer reports.
#[derive(Debug, Clone, PartialEq)]
pub enum RiotError {
    /// 401/403: the API key is missing, invalid or (most likely) expired.
    Auth(String),
    /// 429: back off. `retry_after` is in seconds when Riot told us.
    RateLimited { message: String, retry_after: Option<f64> },
    /// 5xx / timeout / connection failure. Retry with jitter, do not alert.
    Unavailable(String),
    /// 404: unknown Riot ID, PUUID or match.
    NotFound(String),
    /// 400: most often a PUUID encrypted under a different API key (run `repuuid`).
    BadRequest(String),
    /// Any other HTTP status.
    Http(String),
    /// The match payload is unusable (missing timestamps, absurd duration...).
    Parse(String),
    /// A response that doesn't have the shape the API documents. A bug, not an outage.
    Unexpected(String),
}

impl RiotError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Auth(_) => "RiotAuthError",
            Self::RateLimited { .. } => "RiotRateLimited",
            Self::Unavailable(_) => "RiotUnavailable",
            Self::NotFound(_) => "RiotNotFound",
            Self::BadRequest(_) => "RiotBadRequest",
            Self::Http(_) => "RiotError",
            Self::Parse(_) => "MatchParseError",
            Self::Unexpected(_) => "UnexpectedResponse",
        }
    }
}

impl fmt::Display for RiotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimited { message, .. } => f.write_str(message),
            Self::Auth(m)
            | Self::Unavailable(m)
            | Self::NotFound(m)
            | Self::BadRequest(m)
            | Self::Http(m)
            | Self::Parse(m)
            | Self::Unexpected(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for RiotError {}

pub type RiotResult<T> = Result<T, RiotError>;

pub trait Riot: Send + Sync {
    /// Riot ID -> account (with the summoner level when there is a League profile).
    fn resolve_account(&self, game_name: &str, tag_line: &str, platform: &str) -> RiotResult<AccountInfo>;
    /// PUUID -> current Riot ID (names change; the PUUID is the identity).
    fn account_by_puuid(&self, puuid: &str, platform: &str) -> RiotResult<AccountInfo>;
    /// Match ids (`EUW1_123`) that ended after `since_ms`, newest first.
    fn recent_match_ids(
        &self,
        puuid: &str,
        platform: &str,
        since_ms: Option<i64>,
        count: u32,
    ) -> RiotResult<Vec<String>>;
    fn get_match(&self, match_id: &str, platform: &str) -> RiotResult<MatchData>;
    /// Live-game lookup. false when the player has no active game (404).
    fn is_in_game(&self, puuid: &str, platform: &str) -> RiotResult<bool>;
}

// -- routing ------------------------------------------------------------------

/// platform routing value (summoner, spectator) -> regional routing value (account, match)
pub const PLATFORM_TO_REGION: &[(&str, &str)] = &[
    ("na1", "americas"),
    ("br1", "americas"),
    ("la1", "americas"),
    ("la2", "americas"),
    ("euw1", "europe"),
    ("eun1", "europe"),
    ("tr1", "europe"),
    ("ru", "europe"),
    ("me1", "europe"),
    ("kr", "asia"),
    ("jp1", "asia"),
    ("oc1", "sea"),
    ("ph2", "sea"),
    ("sg2", "sea"),
    ("th2", "sea"),
    ("tw2", "sea"),
    ("vn2", "sea"),
];

pub fn region_for(platform: &str) -> Option<&'static str> {
    PLATFORM_TO_REGION.iter().find(|(p, _)| p.eq_ignore_ascii_case(platform)).map(|(_, region)| *region)
}

pub fn is_platform(value: &str) -> bool {
    region_for(value).is_some()
}

pub fn platforms() -> impl Iterator<Item = &'static str> {
    PLATFORM_TO_REGION.iter().map(|(p, _)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_routing_lives_in_one_table() {
        assert_eq!(region_for("EUW1"), Some("europe"));
        assert_eq!(region_for("na1"), Some("americas"));
        assert_eq!(region_for("kr"), Some("asia"));
        assert_eq!(region_for("oc1"), Some("sea"));
        assert!(is_platform("KR") && !is_platform("mars"));
        let mut regions: Vec<&str> = PLATFORM_TO_REGION.iter().map(|(_, r)| *r).collect();
        regions.sort();
        regions.dedup();
        assert_eq!(regions, ["americas", "asia", "europe", "sea"]);
    }
}
