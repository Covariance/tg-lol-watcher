//! Thin HTTP RiotClient. Five endpoints, called directly: account-v1, summoner-v4
//! (level only), match-v5 ids + match, spectator-v5.
//!
//! Retries live in `retry.rs`. This module only maps responses onto typed errors and
//! keeps us inside the application rate limits, so a backfill doesn't burn 429s.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::parsing::parse_match_json;
use super::{RiotError, RiotResult, region_for};
use crate::clock::Pacer;
use crate::http::{Http, Response, encode_segment};
use crate::models::{AccountInfo, MatchData};

/// Dev and personal keys: 20 requests/second and 100 per 2 minutes.
pub const APP_RATE_LIMITS: &[(usize, f64)] = &[(20, 1.0), (100, 120.0)];
const DEFAULT_429_PAUSE_S: f64 = 5.0;

/// Client-side sliding windows over the application rate limits.
///
/// Riot enforces the same windows server-side; staying under them here means a burst
/// (a 100-match backfill) waits instead of collecting 429s. `pause` is how a 429 that
/// does get through holds back every other caller too.
pub struct RateLimiter {
    pacer: Arc<dyn Pacer>,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    windows: Vec<(usize, f64, VecDeque<f64>)>,
    paused_until: f64,
}

impl RateLimiter {
    pub fn new(limits: &[(usize, f64)], pacer: Arc<dyn Pacer>) -> Self {
        let windows = limits.iter().map(|&(n, per)| (n, per, VecDeque::with_capacity(n))).collect();
        Self { pacer, state: Mutex::new(LimiterState { windows, paused_until: f64::MIN }) }
    }

    pub fn pause(&self, seconds: f64) {
        let mut state = self.state.lock().unwrap();
        state.paused_until = state.paused_until.max(self.pacer.monotonic_s() + seconds);
    }

    /// Block until a request may be sent, then record it. Holding the lock while
    /// sleeping is the point: callers queue up behind each other in order.
    pub fn acquire(&self) {
        let mut state = self.state.lock().unwrap();
        loop {
            let now = self.pacer.monotonic_s();
            let mut wait = state.paused_until - now;
            for (n, per, stamps) in &mut state.windows {
                while stamps.front().is_some_and(|&t| t <= now - *per) {
                    stamps.pop_front();
                }
                if stamps.len() >= *n {
                    wait = wait.max(stamps[0] + *per - now);
                }
            }
            if wait <= 0.0 {
                for (_, _, stamps) in &mut state.windows {
                    stamps.push_back(now);
                }
                return;
            }
            self.pacer.sleep(wait);
        }
    }
}

/// account-v1 has no `sea` cluster; Riot serves those accounts from `asia`.
fn account_region(platform: &str) -> RiotResult<&'static str> {
    match region(platform)? {
        "sea" => Ok("asia"),
        region => Ok(region),
    }
}

fn region(platform: &str) -> RiotResult<&'static str> {
    region_for(platform).ok_or_else(|| RiotError::BadRequest(format!("unknown platform {platform:?}")))
}

pub fn split_match_id(match_id: &str) -> Option<(&str, u64)> {
    let (prefix, number) = match_id.rsplit_once('_')?;
    if prefix.is_empty() || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((prefix, number.parse().ok()?))
}

/// Typed error for a failed response, or None for a success.
pub fn map_status(response: &Response, path: &str) -> Option<RiotError> {
    let code = response.status;
    if code < 400 {
        return None;
    }
    let what = format!("HTTP {code} for {path}");
    Some(match code {
        401 | 403 => RiotError::Auth(what),
        404 => RiotError::NotFound(what),
        400 => RiotError::BadRequest(what),
        429 => {
            let kind = response.rate_limit_type.as_deref().unwrap_or("service");
            RiotError::RateLimited {
                message: format!("{what} ({kind} limit)"),
                retry_after: response.retry_after.as_deref().and_then(|v| v.trim().parse().ok()),
            }
        }
        500.. => RiotError::Unavailable(what),
        _ => RiotError::Http(what),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountDto {
    puuid: Option<String>,
    game_name: Option<String>,
    tag_line: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummonerDto {
    summoner_level: Option<i64>,
}

pub struct HttpRiotClient {
    http: Arc<dyn Http>,
    limiter: RateLimiter,
    key: String,
    /// Test hook: `{base}/{routing value}{path}` instead of Riot's hosts.
    base: Option<String>,
}

impl HttpRiotClient {
    pub fn new(api_key: &str, http: Arc<dyn Http>, limiter: RateLimiter) -> Self {
        Self { http, limiter, key: api_key.to_owned(), base: None }
    }

    pub fn with_base_url(mut self, base: Option<String>) -> Self {
        self.base = base.map(|b| b.trim_end_matches('/').to_owned());
        self
    }

    fn get(&self, host: &str, path: &str) -> RiotResult<Vec<u8>> {
        self.limiter.acquire();
        let url = match &self.base {
            None => format!("https://{host}.api.riotgames.com{path}"),
            Some(base) => format!("{base}/{host}{path}"),
        };
        let response = self
            .http
            .get(&url, &[("X-Riot-Token", &self.key)])
            .map_err(|e| RiotError::Unavailable(format!("transport: {}", e.0)))?;
        let route = path.split('?').next().unwrap_or(path);
        if let Some(error) = map_status(&response, route) {
            if let RiotError::RateLimited { retry_after, .. } = &error {
                self.limiter.pause(retry_after.unwrap_or(DEFAULT_429_PAUSE_S));
            }
            return Err(error);
        }
        Ok(response.body)
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, host: &str, path: &str) -> RiotResult<T> {
        let body = self.get(host, path)?;
        serde_json::from_slice(&body).map_err(|e| {
            let route = path.split('?').next().unwrap_or(path);
            if e.is_syntax() || e.is_eof() {
                RiotError::Unavailable(format!("invalid JSON from {route}"))
            } else {
                RiotError::Unexpected(format!("unexpected JSON from {route}: {e}"))
            }
        })
    }

    fn level(&self, puuid: &str, platform: &str) -> Option<i64> {
        let path = format!("/lol/summoner/v4/summoners/by-puuid/{}", encode_segment(puuid));
        match self.get_json::<SummonerDto>(&platform.to_ascii_lowercase(), &path) {
            Ok(dto) => dto.summoner_level,
            Err(e) => {
                // The level is decoration; never fail an /add over it.
                log::debug!("summoner level lookup failed: {e}");
                None
            }
        }
    }
}

impl super::Riot for HttpRiotClient {
    fn resolve_account(&self, game_name: &str, tag_line: &str, platform: &str) -> RiotResult<AccountInfo> {
        let path =
            format!("/riot/account/v1/accounts/by-riot-id/{}/{}", encode_segment(game_name), encode_segment(tag_line));
        let dto: AccountDto = self.get_json(account_region(platform)?, &path)?;
        let puuid = dto.puuid.ok_or_else(|| RiotError::Unexpected("account has no puuid".into()))?;
        let summoner_level = self.level(&puuid, platform);
        Ok(AccountInfo {
            game_name: dto.game_name.filter(|n| !n.is_empty()).unwrap_or_else(|| game_name.into()),
            tag_line: dto.tag_line.filter(|t| !t.is_empty()).unwrap_or_else(|| tag_line.into()),
            puuid,
            summoner_level,
        })
    }

    fn account_by_puuid(&self, puuid: &str, platform: &str) -> RiotResult<AccountInfo> {
        let path = format!("/riot/account/v1/accounts/by-puuid/{}", encode_segment(puuid));
        let dto: AccountDto = self.get_json(account_region(platform)?, &path)?;
        match (dto.game_name.filter(|n| !n.is_empty()), dto.tag_line.filter(|t| !t.is_empty())) {
            (Some(game_name), Some(tag_line)) => {
                Ok(AccountInfo { puuid: puuid.into(), game_name, tag_line, summoner_level: None })
            }
            _ => Err(RiotError::NotFound(format!(
                "account {}… has no Riot ID",
                puuid.chars().take(12).collect::<String>()
            ))),
        }
    }

    fn recent_match_ids(
        &self,
        puuid: &str,
        platform: &str,
        since_ms: Option<i64>,
        count: u32,
    ) -> RiotResult<Vec<String>> {
        let mut path = format!("/lol/match/v5/matches/by-puuid/{}/ids?start=0&count={count}", encode_segment(puuid));
        if let Some(ms) = since_ms {
            // match-v5 startTime is epoch *seconds*
            path.push_str(&format!("&startTime={}", ms.div_euclid(1000)));
        }
        self.get_json(region(platform)?, &path)
    }

    fn get_match(&self, match_id: &str, _platform: &str) -> RiotResult<MatchData> {
        // The id, not the player, names the shard the match lives on.
        let (prefix, _) = split_match_id(match_id)
            .ok_or_else(|| RiotError::BadRequest(format!("malformed match id {match_id:?}")))?;
        let path = format!("/lol/match/v5/matches/{}", encode_segment(match_id));
        let body = self.get(region(prefix)?, &path)?;
        parse_match_json(match_id, &body).map_err(|e| RiotError::Parse(e.0))
    }

    fn is_in_game(&self, puuid: &str, platform: &str) -> RiotResult<bool> {
        let path = format!("/lol/spectator/v5/active-games/by-summoner/{}", encode_segment(puuid));
        match self.get(&platform.to_ascii_lowercase(), &path) {
            Ok(_) => Ok(true),
            Err(RiotError::NotFound(_)) => Ok(false), // 404 is the normal "not in a game" answer
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::TransportError;
    use crate::models::WIN;
    use crate::riot::Riot;
    use crate::testutil::{FakeHttp, FakePacer, fixture};

    fn no_limit() -> RateLimiter {
        RateLimiter::new(&[], Arc::new(FakePacer::default()))
    }

    fn client(http: &Arc<FakeHttp>) -> HttpRiotClient {
        HttpRiotClient::new("RGAPI-test", http.clone(), no_limit())
    }

    const ACCOUNT: &str = r#"{"puuid": "puuid-alice", "gameName": "Alice", "tagLine": "EUW"}"#;

    #[test]
    fn resolve_account_routes_by_region_and_sends_the_key() {
        let http = FakeHttp::with(&[
            ("europe/riot/account/v1/accounts/by-riot-id/Al%20ice/EUW", 200, ACCOUNT),
            ("euw1/lol/summoner/v4/summoners/by-puuid/puuid-alice", 200, r#"{"summonerLevel": 321}"#),
        ]);
        let acct = client(&http).resolve_account("Al ice", "EUW", "euw1").unwrap();
        assert_eq!(
            (acct.puuid.as_str(), acct.game_name.as_str(), acct.tag_line.as_str(), acct.summoner_level),
            ("puuid-alice", "Alice", "EUW", Some(321))
        );
        assert_eq!(http.requests()[0].1, "RGAPI-test");
    }

    #[test]
    fn resolve_account_survives_a_failed_level_lookup() {
        let http = FakeHttp::with(&[("americas/riot/account/v1/accounts/by-riot-id/Bob/NA1", 200, ACCOUNT)]);
        assert_eq!(client(&http).resolve_account("Bob", "NA1", "na1").unwrap().summoner_level, None);
    }

    #[test]
    fn sea_accounts_are_served_from_asia() {
        let http = FakeHttp::with(&[("asia/riot/account/v1/accounts/by-puuid/p", 200, ACCOUNT)]);
        assert_eq!(client(&http).account_by_puuid("p", "oc1").unwrap().game_name, "Alice");
    }

    #[test]
    fn account_without_a_riot_id_is_not_found() {
        let http = FakeHttp::with(&[("europe/riot/account/v1/accounts/by-puuid/p", 200, r#"{"puuid": "p"}"#)]);
        assert!(matches!(client(&http).account_by_puuid("p", "euw1"), Err(RiotError::NotFound(_))));
    }

    #[test]
    fn recent_match_ids_sends_start_time_in_seconds() {
        let http = FakeHttp::with(&[("europe/lol/match/v5/matches/by-puuid/p/ids", 200, r#"["EUW1_2", "EUW1_1"]"#)]);
        let c = client(&http);
        assert_eq!(c.recent_match_ids("p", "euw1", Some(1_767_225_600_123), 5).unwrap(), ["EUW1_2", "EUW1_1"]);
        assert!(http.requests()[0].0.ends_with("/ids?start=0&count=5&startTime=1767225600"));
        c.recent_match_ids("p", "euw1", None, 20).unwrap();
        assert!(!http.requests()[1].0.contains("startTime"));
    }

    #[test]
    fn get_match_parses_info_and_routes_by_the_match_id() {
        let body = String::from_utf8(fixture("match_ranked")).unwrap();
        let http = FakeHttp::with(&[("europe/lol/match/v5/matches/EUW1_7000000001", 200, &body)]);
        let m = client(&http).get_match("EUW1_7000000001", "na1").unwrap(); // the id decides
        assert_eq!(m.participants["puuid-alice"].win, WIN);
        assert_eq!(m.participants["puuid-bob"].champion, "Wukong");
    }

    #[test]
    fn get_match_without_info_is_unparseable() {
        let http = FakeHttp::with(&[("europe/lol/match/v5/matches/EUW1_1", 200, r#"{"metadata": {}}"#)]);
        assert!(matches!(client(&http).get_match("EUW1_1", "euw1"), Err(RiotError::Parse(_))));
    }

    #[test]
    fn spectator_200_is_in_game_and_404_is_not() {
        let http =
            FakeHttp::with(&[("euw1/lol/spectator/v5/active-games/by-summoner/playing", 200, r#"{"gameId": 1}"#)]);
        let c = client(&http);
        assert!(c.is_in_game("playing", "euw1").unwrap());
        assert!(!c.is_in_game("idle", "euw1").unwrap());
        assert_eq!(http.requests().len(), 2); // one call each, no summoner lookup
    }

    #[test]
    fn http_codes_map_to_typed_errors() {
        for (code, kind) in [
            (401, "RiotAuthError"),
            (403, "RiotAuthError"),
            (404, "RiotNotFound"),
            (400, "RiotBadRequest"),
            (429, "RiotRateLimited"),
            (500, "RiotUnavailable"),
            (503, "RiotUnavailable"),
            (418, "RiotError"),
        ] {
            let http = FakeHttp::with(&[("europe/riot/account/v1/accounts/by-puuid/p", code, "{}")]);
            assert_eq!(client(&http).account_by_puuid("p", "euw1").unwrap_err().kind(), kind, "{code}");
        }
    }

    #[test]
    fn every_429_is_a_rate_limit_carrying_retry_after() {
        for kind in [Some("application"), Some("method"), Some("service"), Some("server"), None] {
            let http = Arc::new(FakeHttp::default());
            http.route_full(
                "europe/riot/account/v1/accounts/by-puuid/p",
                Response {
                    status: 429,
                    retry_after: Some("11".into()),
                    rate_limit_type: kind.map(str::to_owned),
                    body: b"{}".to_vec(),
                },
            );
            match client(&http).account_by_puuid("p", "euw1") {
                Err(RiotError::RateLimited { retry_after, .. }) => assert_eq!(retry_after, Some(11.0)),
                other => panic!("{kind:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn network_failures_and_garbage_bodies_are_unavailable() {
        let http = Arc::new(FakeHttp::default());
        http.fail_with(TransportError("connect timeout".into()));
        assert!(matches!(client(&http).is_in_game("p", "euw1"), Err(RiotError::Unavailable(_))));

        let http = FakeHttp::with(&[("europe/riot/account/v1/accounts/by-puuid/p", 200, "<html>")]);
        assert!(matches!(client(&http).account_by_puuid("p", "euw1"), Err(RiotError::Unavailable(_))));
    }

    #[test]
    fn a_429_pauses_the_limiter_for_every_caller() {
        let pacer = Arc::new(FakePacer::default());
        let http = Arc::new(FakeHttp::default());
        http.route_full(
            "europe/riot/account/v1/accounts/by-puuid/p",
            Response {
                status: 429,
                retry_after: Some("7".into()),
                rate_limit_type: Some("application".into()),
                body: vec![],
            },
        );
        let c = HttpRiotClient::new("k", http.clone(), RateLimiter::new(APP_RATE_LIMITS, pacer.clone()));
        assert!(c.account_by_puuid("p", "euw1").is_err());
        assert!(c.is_in_game("p", "euw1").is_ok()); // the next call waits out the pause first
        assert_eq!(pacer.sleeps(), [7.0]);
    }

    #[test]
    fn limiter_waits_for_the_tightest_window() {
        let pacer = Arc::new(FakePacer::default());
        let limiter = RateLimiter::new(&[(2, 1.0), (3, 10.0)], pacer.clone());
        for _ in 0..3 {
            limiter.acquire();
        }
        assert_eq!(pacer.sleeps(), [1.0]); // third call: the 2-per-second window
        limiter.acquire();
        assert_eq!(pacer.sleeps(), [1.0, 9.0]); // fourth: the 3-per-10s window
        assert_eq!(pacer.monotonic_s(), 10.0);
    }

    #[test]
    fn split_match_ids() {
        assert_eq!(split_match_id("EUW1_7000000001"), Some(("EUW1", 7_000_000_001)));
        assert_eq!(split_match_id("garbage"), None);
        assert_eq!(split_match_id("_123"), None);
        assert_eq!(split_match_id("EUW1_12a"), None);
    }
}
