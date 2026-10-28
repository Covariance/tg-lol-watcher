//! The object graph handed to command handlers and jobs.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use jiff::Zoned;

use crate::clock::Clock;
use crate::config::Config;
use crate::db::Database;
use crate::flusher::SessionService;
use crate::notifier::{Alerter, Notifier};
use crate::poller::Poller;
use crate::riot::Riot;
use crate::telegram::Telegram;

pub struct Services {
    pub cfg: Config,
    pub db: Arc<Database>,
    pub riot: Arc<dyn Riot>,
    pub poller: Arc<Poller>,
    pub sessions: Arc<SessionService>,
    pub notifier: Arc<Notifier>,
    pub alerter: Arc<Alerter>,
    pub tg: Arc<dyn Telegram>,
    pub clock: Clock,
    /// The next scheduled weekly run, kept current by the scheduler.
    pub next_weekly: Mutex<Option<Zoned>>,
    /// User ids already logged as strangers (bounded), so a spammer is one log line.
    pub logged_strangers: Mutex<HashSet<i64>>,
}

impl Services {
    pub fn now(&self) -> i64 {
        (self.clock)()
    }
}
