//! Telegram delivery: broadcast to subscribers, and deduped operator alerts.

use std::sync::Arc;

use crate::clock::{Clock, Pacer};
use crate::db::{Database, DbResult};
use crate::formatting::{TELEGRAM_LIMIT, chars, format_game, format_session};
use crate::models::{Match, Player};
use crate::telegram::{Telegram, TgError};

/// Once per hour per alert key.
pub const ALERT_COOLDOWN_MS: i64 = 60 * 60 * 1000;

/// What the session service needs to announce things; faked in tests.
pub trait SessionNotifier: Send + Sync {
    fn send_session(&self, player: &Player, matches: &[Match]) -> DbResult<bool>;
    fn send_game(&self, player: &Player, m: &Match) -> DbResult<bool>;
}

pub struct Notifier {
    tg: Arc<dyn Telegram>,
    db: Arc<Database>,
    pacer: Arc<dyn Pacer>,
}

impl Notifier {
    pub fn new(tg: Arc<dyn Telegram>, db: Arc<Database>, pacer: Arc<dyn Pacer>) -> Self {
        Self { tg, db, pacer }
    }

    /// Send one HTML message. false (and logged) instead of an error.
    pub fn send_to(&self, chat_id: i64, text: &str) -> bool {
        let truncated;
        let mut text = text;
        if chars(text) > TELEGRAM_LIMIT {
            log::error!("message of {} chars exceeds Telegram limit; truncating", chars(text));
            truncated = text.chars().take(TELEGRAM_LIMIT - 1).chain(std::iter::once('…')).collect::<String>();
            text = &truncated;
        }
        for attempt in 1..=2 {
            match self.tg.send_message(chat_id, text, true) {
                Ok(()) => return true,
                Err(TgError::RetryAfter(seconds)) if attempt == 1 => self.pacer.sleep(seconds),
                Err(TgError::RetryAfter(_)) => {
                    log::warn!("telegram flood control persisted for chat {chat_id}");
                    return false;
                }
                Err(TgError::Forbidden(reason)) => {
                    log::warn!("telegram refused chat {chat_id} ({reason}); is the bot blocked/removed?");
                    return false;
                }
                Err(TgError::Other(reason)) => {
                    log::warn!("telegram send to {chat_id} failed: {reason}");
                    return false;
                }
            }
        }
        false
    }

    /// Send to every subscriber. true if delivered somewhere, or if there is nobody to
    /// deliver to (nothing to retry).
    pub fn broadcast(&self, text: &str) -> DbResult<bool> {
        let chats = self.db.subscribers()?;
        if chats.is_empty() {
            log::warn!("no subscribers; message dropped (send /start from an admin chat)");
            return Ok(true);
        }
        let mut any = false;
        for chat in chats {
            any |= self.send_to(chat, text);
        }
        Ok(any)
    }
}

impl SessionNotifier for Notifier {
    fn send_session(&self, player: &Player, matches: &[Match]) -> DbResult<bool> {
        self.broadcast(&format_session(player, matches))
    }

    fn send_game(&self, player: &Player, m: &Match) -> DbResult<bool> {
        self.broadcast(&format_game(player, m))
    }
}

/// Where operator alerts go; the notifier in production, a recorder in tests.
pub trait AlertSink: Send + Sync {
    fn send_to(&self, chat_id: i64, text: &str) -> bool;
}

impl AlertSink for Notifier {
    fn send_to(&self, chat_id: i64, text: &str) -> bool {
        Notifier::send_to(self, chat_id, text)
    }
}

/// Operator alerts (expired key, poll failures) to the admin allowlist, deduped
/// through `kv` so a persistent fault pings at most once an hour.
pub struct Alerter {
    sink: Arc<dyn AlertSink>,
    db: Arc<Database>,
    admins: Vec<i64>,
    clock: Clock,
}

impl Alerter {
    pub fn new(sink: Arc<dyn AlertSink>, db: Arc<Database>, admins: Vec<i64>, clock: Clock) -> Self {
        Self { sink, db, admins, clock }
    }

    /// true if an alert was actually sent (i.e. not deduped). Never fails: an alert
    /// that can't be recorded is still logged.
    pub fn alert(&self, key: &str, text: &str) -> bool {
        let kv_key = format!("alert:{key}");
        let now = (self.clock)();
        match self.db.kv_get(&kv_key) {
            Ok(Some(last)) => {
                if let Ok(last) = last.parse::<i64>()
                    && now - last < ALERT_COOLDOWN_MS
                {
                    log::debug!("alert {key} suppressed (sent {}s ago)", (now - last) / 1000);
                    return false;
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("alert {key}: dedupe lookup failed: {e}"),
        }
        // Record first: if Telegram is down we still don't hammer it every poll.
        if let Err(e) = self.db.kv_set(&kv_key, &now.to_string()) {
            log::error!("alert {key}: could not record: {e}");
        }
        log::warn!("ALERT {key}: {text}");
        for &admin in &self.admins {
            self.sink.send_to(admin, &format!("⚠️ {text}"));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakePacer, FakeTelegram, T0, make_match, make_player, test_db};

    fn notifier(tg: &Arc<FakeTelegram>, db: &Arc<Database>, pacer: &Arc<FakePacer>) -> Notifier {
        Notifier::new(tg.clone(), db.clone(), pacer.clone())
    }

    #[test]
    fn broadcasts_to_every_subscriber() {
        let db = Arc::new(test_db());
        db.add_subscriber(1, T0).unwrap();
        db.add_subscriber(2, T0).unwrap();
        let tg = Arc::new(FakeTelegram::default());
        let n = notifier(&tg, &db, &Arc::new(FakePacer::default()));
        assert!(n.send_session(&make_player(), &[make_match("a", T0, 30)]).unwrap());
        assert_eq!(tg.sent().iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn with_no_subscribers_drops_quietly() {
        let db = Arc::new(test_db());
        let n = notifier(&Arc::new(FakeTelegram::default()), &db, &Arc::new(FakePacer::default()));
        assert!(n.broadcast("hi").unwrap()); // nothing to retry
    }

    #[test]
    fn reports_failure_so_the_session_can_retry() {
        let db = Arc::new(test_db());
        db.add_subscriber(1, T0).unwrap();
        for error in [TgError::Other("network".into()), TgError::Forbidden("bot was blocked".into())] {
            let tg = Arc::new(FakeTelegram::default());
            tg.fail_next([error]);
            assert!(!notifier(&tg, &db, &Arc::new(FakePacer::default())).broadcast("hi").unwrap());
        }
    }

    #[test]
    fn honours_flood_control_once() {
        let db = Arc::new(test_db());
        db.add_subscriber(1, T0).unwrap();
        let (tg, pacer) = (Arc::new(FakeTelegram::default()), Arc::new(FakePacer::default()));
        tg.fail_next([TgError::RetryAfter(3.0)]);
        assert!(notifier(&tg, &db, &pacer).broadcast("hi").unwrap());
        assert_eq!(pacer.sleeps(), [3.0]);
        assert_eq!(tg.sent().len(), 1);
    }

    #[test]
    fn truncates_an_oversized_message_instead_of_failing() {
        let db = Arc::new(test_db());
        db.add_subscriber(1, T0).unwrap();
        let tg = Arc::new(FakeTelegram::default());
        notifier(&tg, &db, &Arc::new(FakePacer::default())).broadcast(&"é".repeat(5000)).unwrap();
        assert!(chars(&tg.sent()[0].1) <= TELEGRAM_LIMIT);
    }
}
