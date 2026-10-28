//! Retry policy wrapper: transient failures are handled here, not in callers.

use std::sync::Arc;

use super::{Riot, RiotError, RiotResult};
use crate::clock::{Pacer, jitter};
use crate::models::{AccountInfo, MatchData};

/// One full 2-minute application window.
const MAX_RATE_LIMIT_WAIT_S: f64 = 130.0;

/// Wraps any Riot client. `Unavailable` retries with jittered backoff; `RateLimited`
/// waits `retry_after` once. Auth/NotFound/BadRequest pass through.
pub struct RetryingClient {
    inner: Arc<dyn Riot>,
    pacer: Arc<dyn Pacer>,
    attempts: u32,
    base_delay_s: f64,
}

impl RetryingClient {
    pub fn new(inner: Arc<dyn Riot>, pacer: Arc<dyn Pacer>) -> Self {
        Self { inner, pacer, attempts: 3, base_delay_s: 2.0 }
    }

    fn call<T>(&self, name: &str, f: impl Fn() -> RiotResult<T>) -> RiotResult<T> {
        let mut rate_limit_retried = false;
        let mut attempt = 0;
        loop {
            match f() {
                Err(RiotError::RateLimited { retry_after, message }) if !rate_limit_retried => {
                    rate_limit_retried = true;
                    let wait = retry_after.unwrap_or(5.0).min(MAX_RATE_LIMIT_WAIT_S);
                    log::warn!("riot {name} rate limited ({message}); waiting {wait:.1}s");
                    self.pacer.sleep(wait);
                }
                Err(RiotError::Unavailable(message)) => {
                    attempt += 1;
                    if attempt >= self.attempts {
                        return Err(RiotError::Unavailable(message));
                    }
                    let delay = self.base_delay_s * f64::from(1u32 << (attempt - 1)) * jitter(0.5, 1.5);
                    log::warn!(
                        "riot {name} unavailable ({message}); retry {attempt}/{} in {delay:.1}s",
                        self.attempts - 1
                    );
                    self.pacer.sleep(delay);
                }
                other => return other,
            }
        }
    }
}

impl Riot for RetryingClient {
    fn resolve_account(&self, game_name: &str, tag_line: &str, platform: &str) -> RiotResult<AccountInfo> {
        self.call("resolve_account", || self.inner.resolve_account(game_name, tag_line, platform))
    }

    fn account_by_puuid(&self, puuid: &str, platform: &str) -> RiotResult<AccountInfo> {
        self.call("account_by_puuid", || self.inner.account_by_puuid(puuid, platform))
    }

    fn recent_match_ids(
        &self,
        puuid: &str,
        platform: &str,
        since_ms: Option<i64>,
        count: u32,
    ) -> RiotResult<Vec<String>> {
        self.call("recent_match_ids", || self.inner.recent_match_ids(puuid, platform, since_ms, count))
    }

    fn get_match(&self, match_id: &str, platform: &str) -> RiotResult<MatchData> {
        self.call("get_match", || self.inner.get_match(match_id, platform))
    }

    fn is_in_game(&self, puuid: &str, platform: &str) -> RiotResult<bool> {
        self.call("is_in_game", || self.inner.is_in_game(puuid, platform))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakePacer, FakeRiot};

    fn retrying(inner: &Arc<FakeRiot>, pacer: &Arc<FakePacer>) -> RetryingClient {
        RetryingClient::new(inner.clone(), pacer.clone())
    }

    fn rate_limited(after: f64) -> RiotError {
        RiotError::RateLimited { message: "429".into(), retry_after: Some(after) }
    }

    #[test]
    fn unavailable_is_retried_with_growing_jittered_backoff() {
        let (inner, pacer) = (Arc::new(FakeRiot::default()), Arc::new(FakePacer::default()));
        inner.fail_in_game([RiotError::Unavailable("503".into()), RiotError::Unavailable("503".into())]);
        inner.set_in_game(Ok(true));
        assert_eq!(retrying(&inner, &pacer).is_in_game("p", "euw1"), Ok(true));
        assert_eq!(inner.in_game_calls(), 3);
        let sleeps = pacer.sleeps();
        assert!(sleeps.len() == 2 && (1.0..=3.0).contains(&sleeps[0]) && (2.0..=6.0).contains(&sleeps[1]));
    }

    #[test]
    fn unavailable_gives_up_after_the_attempt_limit() {
        let (inner, pacer) = (Arc::new(FakeRiot::default()), Arc::new(FakePacer::default()));
        inner.fail_in_game(std::iter::repeat_n(RiotError::Unavailable("x".into()), 5));
        assert!(matches!(retrying(&inner, &pacer).is_in_game("p", "euw1"), Err(RiotError::Unavailable(_))));
        assert_eq!(inner.in_game_calls(), 3);
    }

    #[test]
    fn rate_limit_waits_retry_after_once() {
        let (inner, pacer) = (Arc::new(FakeRiot::default()), Arc::new(FakePacer::default()));
        inner.fail_in_game([rate_limited(7.0)]);
        inner.set_in_game(Ok(true));
        assert_eq!(retrying(&inner, &pacer).is_in_game("p", "euw1"), Ok(true));
        assert_eq!(pacer.sleeps(), [7.0]);

        inner.fail_in_game([rate_limited(1.0), rate_limited(1.0)]);
        assert!(matches!(retrying(&inner, &pacer).is_in_game("p", "euw1"), Err(RiotError::RateLimited { .. })));
    }

    #[test]
    fn definitive_errors_are_never_retried() {
        for error in
            [RiotError::Auth("403".into()), RiotError::NotFound("404".into()), RiotError::BadRequest("400".into())]
        {
            let (inner, pacer) = (Arc::new(FakeRiot::default()), Arc::new(FakePacer::default()));
            inner.fail_in_game([error.clone()]);
            assert_eq!(retrying(&inner, &pacer).is_in_game("p", "euw1"), Err(error));
            assert!(inner.in_game_calls() == 1 && pacer.sleeps().is_empty());
        }
    }
}
