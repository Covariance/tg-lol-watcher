//! lolwatch: watches League of Legends players and posts session and weekly
//! summaries to Telegram.

pub mod app;
pub mod bot;
pub mod cli;
pub mod clock;
pub mod config;
pub mod db;
pub mod error;
pub mod flusher;
pub mod formatting;
pub mod http;
pub mod logger;
pub mod models;
pub mod notifier;
pub mod poller;
pub mod reports;
pub mod riot;
pub mod scheduler;
pub mod services;
pub mod sessions;
pub mod summary;
pub mod telegram;

#[cfg(test)]
mod testutil;
