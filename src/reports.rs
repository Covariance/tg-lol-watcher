//! Glue: DB matches -> aggregated summary -> Telegram messages.

use jiff::Zoned;

use crate::config::Config;
use crate::db::{Database, DbResult};
use crate::formatting::{SPLIT_AT, format_weekly};
use crate::models::Player;
use crate::notifier::Notifier;
use crate::summary::{build_summary, window_days};

/// Summary of the last `days` calendar days (local tz) as message strings. `rolling`
/// uses exactly `days` x 24h of wall-clock instead (see `window_days`).
pub fn render_window(
    db: &Database,
    cfg: &Config,
    players: &[Player],
    days: i64,
    now: Option<Zoned>,
    rolling: bool,
) -> DbResult<Vec<String>> {
    let tz = cfg.tz();
    let now = now.unwrap_or_else(|| Zoned::now().with_time_zone(tz.clone())).with_time_zone(tz.clone());
    let (start, end) = window_days(&now, days, rolling);
    let (start_ms, end_ms) = (start.timestamp().as_millisecond(), end.timestamp().as_millisecond() + 1);
    let per_player = players
        .iter()
        .map(|p| Ok((p.riot_id(), db.matches_between(&p.puuid, start_ms, end_ms)?)))
        .collect::<DbResult<Vec<_>>>()?;
    let summary = build_summary(&start, &end, tz, &per_player, cfg.notifications.gap_ms());
    Ok(format_weekly(&summary, tz, SPLIT_AT, days, rolling))
}

/// Send the last-7-days summary to every subscriber.
/// Returns (messages, messages delivered to at least one chat).
pub fn broadcast_weekly(db: &Database, cfg: &Config, notifier: &Notifier, now: Zoned) -> DbResult<(usize, usize)> {
    let texts = render_window(db, cfg, &db.list_players(true)?, 7, Some(now), true)?;
    let mut delivered = 0;
    for text in &texts {
        delivered += notifier.broadcast(text)? as usize;
    }
    Ok((texts.len(), delivered))
}
