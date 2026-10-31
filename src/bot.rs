//! Telegram commands and the access control around them.
//!
//! Three layers, each failing closed:
//!
//! 1. The gate (`handle_update`): only admins (config.toml) and allow-listed users (the
//!    `allowed_users` table, managed with /allow and /disallow) get past it; everyone
//!    else is ignored *silently* and the update goes no further.
//! 2. `authorize`: every command re-checks the same allow list, then applies its own
//!    rule. Mutating commands are admin-only (never DB-stored users); read commands are
//!    for admins and allowed users.
//! 3. Chat rule: outside a private chat, a command only works in a chat an admin has
//!    subscribed with /start.
//!
//! Messages from bots, or sent on behalf of a chat or channel (anonymous group admins,
//! linked channels), never count as a user, whatever ids are on the list. Allowed users
//! who hit a command above their role get one polite line, never an error and never a
//! hint about what the command would have done. Strangers get nothing.

use std::collections::HashMap;

use crate::config::parse_riot_id;
use crate::db::{DbResult, key_fingerprint};
use crate::error::{Error, Result};
use crate::formatting::{SPLIT_AT, escape, split_message};
use crate::models::Player;
use crate::poller::IngestOptions;
use crate::reports::{broadcast_weekly, render_window};
use crate::riot::{RiotError, is_platform};
use crate::services::Services;
use crate::telegram::{BotCommand, Message, Update, User, parse_command};

pub const REFUSAL: &str = "Sorry, I can't do that here.";
pub const FAILURE: &str = "Something went wrong on my side. It has been logged.";
const MAX_LOGGED_STRANGERS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Subscriber,
    Admin,
}

type Handler = fn(&Ctx) -> Result<()>;

pub struct CommandSpec {
    pub name: &'static str,
    /// One line; also the text in Telegram's menu.
    pub description: &'static str,
    pub access: Access,
    pub handler: Handler,
    /// Arguments, e.g. "GameName#TAG [platform]".
    pub usage: &'static str,
    /// Admin commands that work in a not-yet-subscribed chat (/start).
    pub allow_new_chat: bool,
}

const fn cmd(name: &'static str, description: &'static str, access: Access, handler: Handler) -> CommandSpec {
    CommandSpec { name, description, access, handler, usage: "", allow_new_chat: false }
}

const fn with_usage(mut c: CommandSpec, usage: &'static str) -> CommandSpec {
    c.usage = usage;
    c
}

/// The single source of truth: dispatch, /help and Telegram's command menu are all
/// generated from this, so a command cannot exist without being listed.
pub const COMMANDS: &[CommandSpec] = &[
    cmd("help", "Show this list", Access::Subscriber, help),
    cmd("list", "Tracked players and when they last played", Access::Subscriber, list),
    cmd("status", "Who is in a game right now", Access::Subscriber, status),
    with_usage(cmd("week", "Summary of the last 7 days", Access::Subscriber, week), "[GameName#TAG]"),
    with_usage(
        cmd("stats", "Summary for any window (default 7 days)", Access::Subscriber, stats),
        "GameName#TAG [days]",
    ),
    CommandSpec { allow_new_chat: true, ..cmd("start", "Subscribe this chat to updates", Access::Admin, start) },
    cmd("stop", "Unsubscribe this chat", Access::Admin, stop),
    with_usage(
        cmd("add", "Track a player (imports recent games silently)", Access::Admin, add),
        "GameName#TAG [platform]",
    ),
    with_usage(cmd("remove", "Stop tracking a player (history is kept)", Access::Admin, remove), "GameName#TAG"),
    cmd("sendweekly", "Send the weekly summary to all subscribers now", Access::Admin, sendweekly),
    with_usage(
        cmd("allow", "Let a Telegram user use the bot (or reply to their message)", Access::Admin, allow),
        "[user_id]",
    ),
    with_usage(cmd("disallow", "Stop the bot answering a user", Access::Admin, disallow), "[user_id]"),
    cmd("allowed", "Who may use the bot", Access::Admin, allowed),
    cmd("settings", "Effective config, key status, next weekly run", Access::Admin, settings),
];

/// One command invocation.
pub struct Ctx<'a> {
    pub svc: &'a Services,
    pub msg: &'a Message,
    pub user: &'a User,
    pub args: Vec<String>,
}

impl Ctx<'_> {
    /// Lists grow with the number of players and users; past Telegram's limit the
    /// whole message would be rejected, so long replies go out in line-aligned parts.
    fn reply(&self, text: &str) -> Result<()> {
        for part in split_message(text, SPLIT_AT) {
            self.send(&part, true);
        }
        Ok(())
    }

    fn send(&self, text: &str, html: bool) {
        if let Err(e) = self.svc.tg.send_message(self.msg.chat.id, text, html) {
            log::warn!("reply to chat {} failed: {e}", self.msg.chat.id);
        }
    }
}

// -- access control -----------------------------------------------------------

/// The sending user, or None for anything that isn't a real person speaking as
/// themselves (no user, a bot, or a message sent on behalf of a chat).
fn human_sender(msg: &Message) -> Option<&User> {
    msg.from.as_ref().filter(|u| !u.is_bot && msg.sender_chat.is_none())
}

/// The allow-list check: an admin, or a user an admin has allowed.
fn may_use_bot(svc: &Services, msg: &Message) -> DbResult<bool> {
    match human_sender(msg) {
        None => Ok(false),
        Some(user) => Ok(svc.cfg.is_admin(user.id) || svc.db.is_allowed_user(user.id)?),
    }
}

fn log_stranger(svc: &Services, msg: &Message) {
    let Some(uid) = msg.from.as_ref().map(|u| u.id) else {
        return;
    };
    let mut seen = svc.logged_strangers.lock().unwrap_or_else(|p| p.into_inner());
    if seen.len() >= MAX_LOGGED_STRANGERS || !seen.insert(uid) {
        return;
    }
    log::warn!(
        "ignored a message from user_id={uid} in chat_id={} (not on the allow list; an admin can run /allow {uid})",
        msg.chat.id
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Say nothing at all (strangers, bots, on-behalf-of-chat senders).
    Ignore,
    /// One polite line.
    Refuse,
    Run,
}

/// Layers 2 and 3 for one command.
pub fn authorize(svc: &Services, msg: &Message, spec: &CommandSpec) -> DbResult<Decision> {
    let Some(user) = human_sender(msg) else {
        return Ok(Decision::Ignore);
    };
    if !may_use_bot(svc, msg)? {
        return Ok(Decision::Ignore);
    }
    if spec.access == Access::Admin && !svc.cfg.is_admin(user.id) {
        return Ok(Decision::Refuse);
    }
    let new_chat_ok = spec.access == Access::Admin && spec.allow_new_chat;
    if !new_chat_ok && !msg.chat.is_private() && !svc.db.is_subscriber(msg.chat.id)? {
        return Ok(Decision::Refuse);
    }
    Ok(Decision::Run)
}

/// Everything a received update goes through.
pub fn handle_update(svc: &Services, update: &Update, bot_username: &str) {
    let Some(msg) = &update.message else {
        return;
    };
    // Telegram's own notice that a group became a supergroup (a new chat id). It only
    // moves an existing subscription, so it needs no sender check.
    if let Some(from) = msg.migrate_from_chat_id {
        match svc.db.migrate_subscriber(from, msg.chat.id) {
            Ok(true) => log::info!("group {from} became supergroup {}; subscription moved", msg.chat.id),
            Ok(false) => {}
            Err(e) => log::error!("could not move the subscription of group {from}: {e}"),
        }
        return;
    }
    // Layer 1, the gate. A failing check must block, not wave everything through.
    match may_use_bot(svc, msg) {
        Ok(true) => {}
        Ok(false) => return log_stranger(svc, msg),
        Err(e) => return log::error!("access check failed; refusing the update: {e}"),
    }
    let Some(command) = parse_command(msg, bot_username) else {
        return;
    };
    if let Some(spec) = COMMANDS.iter().find(|c| c.name == command.name) {
        run_command(svc, msg, spec, command.args);
    }
}

/// Authorize, then run one command. A crash becomes a generic line, never details.
pub fn run_command(svc: &Services, msg: &Message, spec: &CommandSpec, args: Vec<String>) {
    let reply_plain = |text: &str| {
        if let Err(e) = svc.tg.send_message(msg.chat.id, text, false) {
            log::warn!("reply to chat {} failed: {e}", msg.chat.id);
        }
    };
    match authorize(svc, msg, spec) {
        Ok(Decision::Ignore) => log_stranger(svc, msg),
        Ok(Decision::Refuse) => reply_plain(REFUSAL),
        Ok(Decision::Run) => {
            let user = human_sender(msg).expect("authorized messages have a sender");
            let ctx = Ctx { svc, msg, user, args };
            if let Err(e) = (spec.handler)(&ctx) {
                log::error!("command handler /{} failed: {}: {e}", spec.name, e.kind());
                reply_plain(FAILURE);
            }
        }
        Err(e) => {
            log::error!("command /{}: access check failed: {e}", spec.name);
            reply_plain(FAILURE);
        }
    }
}

// -- helpers ------------------------------------------------------------------

/// `Game Name#TAG [platform]` -> (name, tag, platform). Names may contain spaces.
fn parse_player_args(args: &[String], default_platform: &str) -> Option<(String, String, String)> {
    let (platform, rest) = match args {
        [rest @ .., last] if !rest.is_empty() && is_platform(last) => (last.to_lowercase(), rest),
        _ => (default_platform.to_owned(), args),
    };
    let (name, tag) = parse_riot_id(&rest.join(" "))?;
    Some((name, tag, platform))
}

pub fn ago(ms: Option<i64>, now: i64) -> String {
    let Some(ms) = ms else {
        return "never".into();
    };
    let minutes = ((now - ms) / 60_000).max(0);
    if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 48 * 60 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / 1440)
    }
}

fn plain_digits(text: &str, max_len: usize) -> bool {
    !text.is_empty() && text.len() <= max_len && text.bytes().all(|b| b.is_ascii_digit())
}

fn tracked(ctx: &Ctx, args: &[String]) -> Result<std::result::Result<Player, &'static str>> {
    let Some((name, tag)) = parse_riot_id(&args.join(" ")) else {
        return Ok(Err("usage"));
    };
    Ok(ctx.svc.db.find_player(&name, &tag, true)?.ok_or("untracked"))
}

const UNTRACKED: &str = "That player isn't being tracked.";

// -- subscription -------------------------------------------------------------

fn start(ctx: &Ctx) -> Result<()> {
    let added = ctx.svc.db.add_subscriber(ctx.msg.chat.id, ctx.svc.now())?;
    ctx.reply(if added {
        "✅ This chat will now get session and weekly updates."
    } else {
        "This chat is already subscribed."
    })
}

fn stop(ctx: &Ctx) -> Result<()> {
    let removed = ctx.svc.db.remove_subscriber(ctx.msg.chat.id)?;
    ctx.reply(if removed { "Unsubscribed. Send /start to resume." } else { "This chat wasn't subscribed." })
}

// -- player management --------------------------------------------------------

fn add(ctx: &Ctx) -> Result<()> {
    let svc = ctx.svc;
    let Some((name, tag, platform)) = parse_player_args(&ctx.args, &svc.cfg.default_platform) else {
        return ctx.reply("Usage: <code>/add GameName#TAG [platform]</code>");
    };
    let acct = match svc.riot.resolve_account(&name, &tag, &platform) {
        Ok(acct) => acct,
        Err(RiotError::NotFound(_)) => {
            return ctx.reply(&format!(
                "No account <b>{}#{}</b> found (platform <code>{platform}</code>).",
                escape(&name),
                escape(&tag)
            ));
        }
        Err(RiotError::Auth(_)) => {
            return ctx.reply("The Riot API key was rejected (likely expired). Update it and restart the bot.");
        }
        Err(e) => {
            log::warn!("/add: riot error {}: {e}", e.kind());
            return ctx.reply("Riot's API isn't responding right now. Try again shortly.");
        }
    };

    if let Some(existing) = svc.db.get_player(&acct.puuid)?.filter(|p| p.active) {
        return ctx.reply(&format!("Already tracking <b>{}</b>.", escape(&existing.riot_id())));
    }
    let Some(level) = acct.summoner_level else {
        // Account lookups work region-wide, so a wrong platform in the same region gets
        // this far. Tracking it would silently break the live-game check and game ids.
        let (n, t) = (escape(&acct.game_name), escape(&acct.tag_line));
        return ctx.reply(&format!(
            "Found the Riot account <b>{n}#{t}</b>, but no League profile on <code>{platform}</code>. \
             Wrong platform? Try <code>/add {n}#{t} &lt;platform&gt;</code>. (If the platform is right, \
             Riot may be flaky: try again in a minute.)"
        ));
    };

    let now = svc.now();
    let player = Player {
        puuid: acct.puuid.clone(),
        game_name: acct.game_name,
        tag_line: acct.tag_line,
        platform: platform.clone(),
        active: true,
        added_by: Some(ctx.user.id),
        added_at: now,
        last_seen_ms: None,
        name_checked_at: Some(now),
    };
    svc.db.add_player(&player)?;
    let level = if level != 0 { format!(", level {level}") } else { String::new() };
    ctx.reply(&format!(
        "✅ Now tracking <b>{}</b> ({platform}{level}). Importing recent games…",
        escape(&player.riot_id())
    ))?;

    let stored = svc.db.get_player(&acct.puuid)?.ok_or_else(|| Error::Other("player vanished after /add".into()))?;
    // Silent: history from before we started watching is never announced.
    let opts = IngestOptions { notify: Some(false), count: Some(100), ..Default::default() };
    match svc.poller.ingest(&stored, &mut HashMap::new(), opts) {
        Ok(n) => ctx.reply(&format!("Imported {n} recent game{}.", if n == 1 { "" } else { "s" })),
        Err(Error::Riot(e)) => {
            log::warn!("/add: initial import failed: {e}");
            ctx.reply("Couldn't import history yet; the poller will retry.")
        }
        Err(e) => Err(e),
    }
}

fn remove(ctx: &Ctx) -> Result<()> {
    match tracked(ctx, &ctx.args)? {
        Err("usage") => ctx.reply("Usage: <code>/remove GameName#TAG</code>"),
        Err(_) => ctx.reply(UNTRACKED),
        Ok(player) => {
            ctx.svc.db.deactivate_player(&player.puuid)?;
            ctx.reply(&format!("Stopped tracking <b>{}</b> (history kept).", escape(&player.riot_id())))
        }
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    let players = ctx.svc.db.list_players(true)?;
    if players.is_empty() {
        return ctx.reply("No players tracked yet. Add one with /add GameName#TAG");
    }
    let now = ctx.svc.now();
    let last_games = ctx.svc.db.last_game_ends()?; // not last_seen_ms: that is a polling marker
    let lines: Vec<String> = players
        .iter()
        .map(|p| {
            format!(
                "• <b>{}</b> ({}) — last game {}",
                escape(&p.riot_id()),
                p.platform,
                ago(last_games.get(&p.puuid).copied(), now)
            )
        })
        .collect();
    ctx.reply(&format!("<b>Tracked players</b>\n{}", lines.join("\n")))
}

fn status(ctx: &Ctx) -> Result<()> {
    let players = ctx.svc.db.list_players(true)?;
    if players.is_empty() {
        return ctx.reply("No players tracked yet.");
    }
    let riot = &ctx.svc.riot;
    // In parallel: the rate limiter still paces them, but the round trips overlap.
    let lines: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> =
            players.iter().map(|p| scope.spawn(move || (p, riot.is_in_game(&p.puuid, &p.platform)))).collect();
        handles
            .into_iter()
            .map(|h| {
                let (p, live) = h.join().expect("status lookups don't panic");
                let who = escape(&p.riot_id());
                match live {
                    Ok(true) => format!("🟢 <b>{who}</b> — in game"),
                    Ok(false) => format!("⚪ <b>{who}</b> — not in game"),
                    Err(e) => {
                        log::info!("/status: {} for {}", e.kind(), p.riot_id());
                        format!("❔ <b>{who}</b> — unknown (Riot API error)")
                    }
                }
            })
            .collect()
    });
    ctx.reply(&lines.join("\n"))
}

// -- stats ----------------------------------------------------------------------

fn send_window(ctx: &Ctx, players: &[Player], days: i64) -> Result<()> {
    for text in render_window(&ctx.svc.db, &ctx.svc.cfg, players, days, None, false)? {
        ctx.reply(&text)?;
    }
    Ok(())
}

fn week(ctx: &Ctx) -> Result<()> {
    let players = if ctx.args.is_empty() {
        ctx.svc.db.list_players(true)?
    } else {
        match tracked(ctx, &ctx.args)? {
            Err("usage") => return ctx.reply("Usage: <code>/week [GameName#TAG]</code>"),
            Err(_) => return ctx.reply(UNTRACKED),
            Ok(player) => vec![player],
        }
    };
    send_window(ctx, &players, 7)
}

fn stats(ctx: &Ctx) -> Result<()> {
    let mut args: &[String] = &ctx.args;
    let mut days = 7;
    if let [rest @ .., last] = args
        && !rest.is_empty()
        && plain_digits(last, 4)
    {
        days = last.parse().expect("checked digits");
        args = rest;
    }
    if !(1..=365).contains(&days) {
        return ctx.reply("Days must be between 1 and 365.");
    }
    match tracked(ctx, args)? {
        Err("usage") => ctx.reply("Usage: <code>/stats GameName#TAG [days]</code>"),
        Err(_) => ctx.reply(UNTRACKED),
        Ok(player) => send_window(ctx, &[player], days),
    }
}

// -- admin ----------------------------------------------------------------------

fn sendweekly(ctx: &Ctx) -> Result<()> {
    let svc = ctx.svc;
    let subscribers = svc.db.subscribers()?;
    if subscribers.is_empty() {
        return ctx.reply("No subscribers yet. Send /start in the chat that should receive updates.");
    }
    let now = jiff::Timestamp::from_millisecond(svc.now()).unwrap_or_else(|_| jiff::Timestamp::now());
    let (messages, delivered) = broadcast_weekly(&svc.db, &svc.cfg, &svc.notifier, now.to_zoned(svc.cfg.tz().clone()))?;
    if delivered < messages {
        return ctx
            .reply(&format!("⚠️ Only {delivered} of {messages} message(s) were delivered to anyone. Check the logs."));
    }
    let n = subscribers.len();
    ctx.reply(&format!("✅ Weekly summary sent to {n} chat{}.", if n == 1 { "" } else { "s" }))
}

/// A numeric Telegram user id from the arguments, or from the message being replied
/// to. None when neither is usable (bots and non-numeric text are rejected).
fn target_user_id(msg: &Message, args: &[String]) -> Option<i64> {
    match args {
        [] => msg.reply_to_message.as_ref()?.from.as_ref().filter(|u| !u.is_bot).map(|u| u.id),
        [text] if plain_digits(text, 15) => text.parse().ok().filter(|&id| id != 0),
        _ => None,
    }
}

fn allow(ctx: &Ctx) -> Result<()> {
    let Some(target) = target_user_id(ctx.msg, &ctx.args) else {
        return ctx
            .reply("Usage: <code>/allow &lt;user_id&gt;</code>, or reply to their message with <code>/allow</code>.");
    };
    if ctx.svc.cfg.is_admin(target) {
        return ctx.reply("That user is an admin already.");
    }
    let added = ctx.svc.db.add_allowed_user(target, Some(ctx.user.id), ctx.svc.now())?;
    log::info!("allow list: {} {target} (by admin {})", if added { "added" } else { "kept" }, ctx.user.id);
    ctx.reply(&if added {
        format!("✅ User <code>{target}</code> may now use the bot.")
    } else {
        format!("User <code>{target}</code> was already allowed.")
    })
}

fn disallow(ctx: &Ctx) -> Result<()> {
    let Some(target) = target_user_id(ctx.msg, &ctx.args) else {
        return ctx.reply(
            "Usage: <code>/disallow &lt;user_id&gt;</code>, or reply to their message with <code>/disallow</code>.",
        );
    };
    if ctx.svc.cfg.is_admin(target) {
        return ctx.reply("Admins are set in config.toml and can't be removed here.");
    }
    let removed = ctx.svc.db.remove_allowed_user(target)?;
    log::info!("allow list: {} {target} (by admin {})", if removed { "removed" } else { "absent" }, ctx.user.id);
    ctx.reply(&if removed {
        format!("Removed <code>{target}</code>; the bot now ignores them.")
    } else {
        format!("User <code>{target}</code> wasn't on the list.")
    })
}

fn allowed(ctx: &Ctx) -> Result<()> {
    let mut lines = vec!["<b>Admins</b> (config.toml)".to_owned()];
    lines.extend(ctx.svc.cfg.admin_user_ids.iter().map(|a| format!("• <code>{a}</code>")));
    lines.push(String::new());
    lines.push("<b>Allowed users</b>".into());
    let users = ctx.svc.db.allowed_users()?;
    if users.is_empty() {
        lines.push("(none yet: /allow &lt;user_id&gt;)".into());
    }
    lines.extend(users.iter().map(|u| format!("• <code>{u}</code>")));
    ctx.reply(&lines.join("\n"))
}

fn settings(ctx: &Ctx) -> Result<()> {
    let svc = ctx.svc;
    let (n, w) = (&svc.cfg.notifications, &svc.cfg.weekly);
    let next = svc.next_weekly.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let next_text = match next {
        Some(next) => next.with_time_zone(svc.cfg.tz().clone()).strftime("%a %d %b %H:%M %Z").to_string(),
        None if !w.enabled => "disabled".into(),
        None => "unknown".into(),
    };
    let key_status = match svc.db.kv_get("alert:riot_auth")?.and_then(|v| v.parse().ok()) {
        Some(last) => format!("⚠️ rejected recently (last alert {})", ago(Some(last), svc.now())),
        None => "ok (no auth failures logged)".into(),
    };
    let lines = [
        "<b>Settings</b>".to_owned(),
        format!(
            "Notifications: <code>{}</code>, gap {}m, recheck {}m, max postpones {}",
            n.mode.as_str(),
            n.session_gap_minutes,
            n.flush_recheck_minutes,
            n.max_postpones
        ),
        format!(
            "Polling: every {}s, default platform <code>{}</code>",
            svc.cfg.poll_interval_seconds, svc.cfg.default_platform
        ),
        format!(
            "Weekly: {} — {} {:02}:{:02} {}",
            if w.enabled { "on" } else { "off" },
            w.day,
            w.hour,
            w.minute,
            w.timezone
        ),
        format!("Next weekly run: {next_text}"),
        format!("Riot key: <code>{}</code> — {key_status}", &key_fingerprint(&svc.cfg.riot_api_key)[..8]),
        format!("Subscribers: {}, players: {}", svc.db.subscribers()?.len(), svc.db.list_players(true)?.len()),
    ];
    ctx.reply(&lines.join("\n"))
}

fn help(ctx: &Ctx) -> Result<()> {
    let line = |c: &CommandSpec| {
        format!("{} — {}", format!("/{} {}", c.name, escape(c.usage)).trim_end(), escape(c.description))
    };
    let mut lines = vec!["<b>Commands</b>".to_owned()];
    lines.extend(COMMANDS.iter().filter(|c| c.access == Access::Subscriber).map(line));
    if ctx.svc.cfg.is_admin(ctx.user.id) {
        lines.push(String::new());
        lines.push("<b>Admin</b>".into());
        lines.extend(COMMANDS.iter().filter(|c| c.access == Access::Admin).map(line));
    }
    ctx.reply(&lines.join("\n"))
}

// -- Telegram's command menu ------------------------------------------------------

/// Commands for Telegram's autocomplete menu (the list shown when typing `/`).
pub fn menu(admin: bool) -> Vec<BotCommand> {
    COMMANDS
        .iter()
        .filter(|c| admin || c.access == Access::Subscriber)
        .map(|c| BotCommand { command: c.name.into(), description: c.description.into() })
        .collect()
}

/// Everyone sees the read commands; each admin's private chat also gets the admin
/// ones. Failures are logged, never fatal: the menu is a convenience.
pub fn publish_menu(svc: &Services) {
    let result = svc.tg.set_my_commands(&menu(false), None).and_then(|()| {
        svc.cfg.admin_user_ids.iter().try_for_each(|&admin| svc.tg.set_my_commands(&menu(true), Some(admin)))
    });
    if let Err(e) = result {
        log::warn!("could not publish the command menu: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::now_ms;
    use crate::models::AccountInfo;
    use crate::testutil::{BotEnv, MIN, T0, make_match, make_player_full};

    const ADMIN: i64 = 1;
    const MEMBER: i64 = 50;
    const STRANGER: i64 = 99;
    const GROUP: i64 = -1001;
    const PRIVATE: i64 = 5;

    fn account(name: &str, tag: &str, puuid: &str, level: Option<i64>) -> AccountInfo {
        AccountInfo { puuid: puuid.into(), game_name: name.into(), tag_line: tag.into(), summoner_level: level }
    }

    fn env() -> BotEnv {
        let env = BotEnv::new(&[ADMIN]);
        env.svc.db.add_allowed_user(MEMBER, Some(ADMIN), 0).unwrap();
        env
    }

    #[test]
    fn non_admin_cannot_mutate_and_gets_one_polite_line() {
        let env = env();
        for (command, args) in [
            ("add", "Faker#KR1"),
            ("remove", "Alice#EUW"),
            ("start", ""),
            ("stop", ""),
            ("settings", ""),
            ("sendweekly", ""),
            ("allow", "77"),
            ("disallow", "50"),
            ("allowed", ""),
        ] {
            assert_eq!(env.call(command, args, MEMBER, PRIVATE, "private"), [REFUSAL], "{command}");
        }
        assert!(env.svc.db.subscribers().unwrap().is_empty());
        assert_eq!(env.svc.db.list_players(true).unwrap().len(), 1);
        assert_eq!(env.svc.db.allowed_users().unwrap(), [MEMBER]);
    }

    #[test]
    fn strangers_are_ignored_completely_on_every_command() {
        let env = env();
        for c in COMMANDS {
            assert!(env.call(c.name, "77", STRANGER, STRANGER, "private").is_empty(), "{}", c.name);
            assert!(env.call(c.name, "77", STRANGER, GROUP, "supergroup").is_empty(), "{}", c.name);
        }
        assert!(env.svc.db.subscribers().unwrap().is_empty());
        assert_eq!(env.svc.db.allowed_users().unwrap(), [MEMBER]);
    }

    #[test]
    fn group_chats_are_refused_until_explicitly_subscribed() {
        let env = env();
        assert_eq!(env.call("add", "Faker#KR1", ADMIN, GROUP, "group"), [REFUSAL]);
        assert_eq!(env.call("list", "", MEMBER, GROUP, "supergroup"), [REFUSAL]);

        let reply = env.call("start", "", ADMIN, GROUP, "group"); // the admin opts in
        assert!(reply[0].contains("will now get"));
        assert!(env.svc.db.is_subscriber(GROUP).unwrap());

        assert!(env.call("list", "", MEMBER, GROUP, "group")[0].contains("Alice#EUW")); // allowed users may read
        // ...but being in a subscribed group is not enough: the sender must be allowed.
        assert!(env.call("list", "", STRANGER, GROUP, "group").is_empty());
    }

    #[test]
    fn read_commands_need_an_allowed_user_in_private_or_a_subscribed_chat() {
        let env = env();
        assert!(env.call("list", "", STRANGER, STRANGER, "private").is_empty());
        assert!(env.call("list", "", ADMIN, PRIVATE, "private")[0].contains("Alice#EUW"));
        assert!(env.call("list", "", MEMBER, MEMBER, "private")[0].contains("Alice#EUW"));
    }

    #[test]
    fn refusal_reveals_nothing_about_the_command() {
        let env = env();
        let reply = env.call("settings", "", MEMBER, PRIVATE, "private");
        assert_eq!(reply, [REFUSAL]);
        assert!(!reply[0].to_lowercase().contains("key") && !reply[0].contains("mode"));
    }

    #[test]
    fn access_matrix_for_every_command() {
        let env = env();
        env.svc.db.add_subscriber(-2002, 0).unwrap(); // a subscribed group
        let chats =
            [("private", PRIVATE, "private"), ("new group", GROUP, "group"), ("subscribed group", -2002, "supergroup")];
        for c in COMMANDS {
            for (who, user) in [("stranger", STRANGER), ("member", MEMBER), ("admin", ADMIN)] {
                for (where_, chat, kind) in chats {
                    let msg = BotEnv::message(&format!("/{}", c.name), user, chat, kind);
                    let want = if who == "stranger" {
                        Decision::Ignore
                    } else if (c.access == Access::Admin && who == "member")
                        || (where_ == "new group" && !c.allow_new_chat)
                    {
                        Decision::Refuse
                    } else {
                        Decision::Run
                    };
                    assert_eq!(authorize(&env.svc, &msg, c).unwrap(), want, "/{} as {who} in {where_}", c.name);
                }
            }
        }
    }

    #[test]
    fn bots_and_on_behalf_of_chat_senders_never_count_even_if_listed() {
        let env = env();
        for c in COMMANDS {
            for user in [ADMIN, MEMBER] {
                let mut bot = BotEnv::message(&format!("/{}", c.name), user, PRIVATE, "private");
                bot.from.as_mut().unwrap().is_bot = true;
                assert_eq!(authorize(&env.svc, &bot, c).unwrap(), Decision::Ignore);
                let mut anon = BotEnv::message(&format!("/{}", c.name), user, PRIVATE, "private");
                anon.sender_chat = Some(crate::telegram::Chat { id: -100, kind: "channel".into() });
                assert_eq!(authorize(&env.svc, &anon, c).unwrap(), Decision::Ignore);
                assert!(env.deliver(bot).is_empty() && env.deliver(anon).is_empty());
            }
        }
    }

    #[test]
    fn a_failing_allow_list_lookup_refuses_instead_of_crashing_open() {
        let env = env();
        env.svc.db.break_allowed_users_for_tests();
        assert!(env.call("list", "", MEMBER, MEMBER, "private").is_empty()); // the gate drops it
        let msg = BotEnv::message("/list", MEMBER, MEMBER, "private");
        run_command(&env.svc, &msg, &COMMANDS[1], vec![]); // past the gate: a generic line
        assert_eq!(env.replies(), [FAILURE]);
    }

    #[test]
    fn a_group_upgrade_notice_moves_the_subscription_whoever_sent_it() {
        let env = env();
        env.svc.db.add_subscriber(GROUP, 0).unwrap();
        let mut notice = BotEnv::message("", STRANGER, -1009999, "supergroup");
        notice.text = None;
        notice.migrate_from_chat_id = Some(GROUP);
        assert!(env.deliver(notice).is_empty());
        assert_eq!(env.svc.db.subscribers().unwrap(), [-1009999]);
    }

    #[test]
    fn a_command_addressed_to_another_bot_or_unknown_is_ignored() {
        let env = env();
        assert!(env.call_raw("/list@someotherbot", ADMIN, PRIVATE, "private").is_empty());
        assert!(env.call_raw("/nosuchcommand", ADMIN, PRIVATE, "private").is_empty());
        assert!(!env.call_raw("/LIST@tb", ADMIN, PRIVATE, "private").is_empty());
    }

    #[test]
    fn allow_and_disallow_control_who_the_bot_answers() {
        let env = env();
        assert!(env.call("list", "", 77, 77, "private").is_empty());
        let reply = env.call("allow", "77", ADMIN, PRIVATE, "private");
        assert!(reply[0].contains("77") && reply[0].contains("may now use"));
        assert!(env.call("list", "", 77, 77, "private")[0].contains("Alice#EUW"));
        assert!(env.call("allow", "77", ADMIN, PRIVATE, "private")[0].contains("already allowed"));
        assert_eq!(env.svc.db.allowed_users().unwrap(), [MEMBER, 77]);

        assert!(env.call("disallow", "77", ADMIN, PRIVATE, "private")[0].contains("Removed"));
        assert!(env.call("list", "", 77, 77, "private").is_empty());
        assert!(env.call("disallow", "77", ADMIN, PRIVATE, "private")[0].contains("wasn't on the list"));
    }

    #[test]
    fn allow_accepts_a_reply_to_a_message_but_not_to_a_bot() {
        let env = env();
        let reply_to = |id: i64, is_bot: bool| {
            let mut msg = BotEnv::message("/allow", ADMIN, PRIVATE, "private");
            msg.reply_to_message = Some(Box::new(BotEnv::message("hi", id, PRIVATE, "private")));
            msg.reply_to_message.as_mut().unwrap().from.as_mut().unwrap().is_bot = is_bot;
            env.deliver(msg)
        };
        assert!(reply_to(123, false)[0].contains("may now use"));
        assert!(env.svc.db.allowed_users().unwrap().contains(&123));
        assert!(reply_to(456, true)[0].contains("Usage"));
        assert!(!env.svc.db.allowed_users().unwrap().contains(&456));
    }

    #[test]
    fn allow_rejects_garbage_ids_and_admins() {
        let env = env();
        for bad in ["abc", "-5", "0", "1.5", "12345678901234567890", "²", "1 2", ""] {
            assert!(env.call("allow", bad, ADMIN, PRIVATE, "private")[0].contains("Usage"), "{bad:?}");
        }
        assert!(env.call("allow", "1", ADMIN, PRIVATE, "private")[0].contains("admin already"));
        assert!(env.call("disallow", "1", ADMIN, PRIVATE, "private")[0].contains("can't be removed"));
        assert_eq!(env.svc.db.allowed_users().unwrap(), [MEMBER]);
    }

    #[test]
    fn allowed_lists_admins_and_users() {
        let text = env().call("allowed", "", ADMIN, PRIVATE, "private").remove(0);
        assert!(text.contains("<code>1</code>") && text.contains("<code>50</code>"));
    }

    #[test]
    fn an_allowed_user_in_a_subscribed_group_can_read_but_not_manage() {
        let env = env();
        env.svc.db.add_subscriber(GROUP, 0).unwrap();
        assert!(env.call("list", "", MEMBER, GROUP, "group")[0].contains("Alice#EUW"));
        assert_eq!(env.call("allow", "77", MEMBER, GROUP, "group"), [REFUSAL]);
    }

    #[test]
    fn start_and_stop_manage_subscription() {
        let env = env();
        env.call("start", "", ADMIN, PRIVATE, "private");
        assert!(env.svc.db.is_subscriber(PRIVATE).unwrap());
        assert!(env.call("start", "", ADMIN, PRIVATE, "private")[0].contains("already"));
        env.call("stop", "", ADMIN, PRIVATE, "private");
        assert!(!env.svc.db.is_subscriber(PRIVATE).unwrap());
    }

    #[test]
    fn a_handler_crash_becomes_a_generic_message_not_details() {
        let env = env();
        env.svc.db.break_players_table_for_tests();
        assert_eq!(env.call("list", "", ADMIN, PRIVATE, "private"), [FAILURE]);
    }

    // -- /add /remove ---------------------------------------------------------------

    #[test]
    fn add_resolves_stores_and_imports_history_silently() {
        let env = env();
        env.riot.set_account("faker", "kr1", account("Faker", "KR1", "puuid-faker", Some(712)));
        env.riot.set_ids("puuid-faker", &["KR_1"]);
        env.riot.add_match(crate::testutil::match_data("KR_1", T0, 30, &["puuid-faker"]));

        let replies = env.call("add", "Faker#KR1 kr", ADMIN, PRIVATE, "private");
        assert!(replies[0].contains("Now tracking") && replies[0].contains("level 712") && replies[0].contains("kr"));
        assert!(replies[1].contains("Imported 1 recent game."));

        let player = env.svc.db.get_player("puuid-faker").unwrap().unwrap();
        assert_eq!((player.platform.as_str(), player.added_by), ("kr", Some(ADMIN)));
        assert!(env.svc.db.open_sessions(None).unwrap().is_empty()); // old games are never announced
    }

    #[test]
    fn add_accepts_names_with_spaces_and_defaults_the_platform() {
        let env = env();
        env.riot.set_account("some name", "euw", account("Some Name", "EUW", "puuid-sn", Some(30)));
        assert!(env.call("add", "Some Name#EUW", ADMIN, PRIVATE, "private")[0].contains("Some Name#EUW"));
        assert_eq!(env.svc.db.get_player("puuid-sn").unwrap().unwrap().platform, "euw1");
    }

    #[test]
    fn add_bad_usage_unknown_account_and_duplicates() {
        let env = env();
        assert!(env.call("add", "no-tag-here", ADMIN, PRIVATE, "private")[0].contains("Usage"));
        assert!(env.call("add", "Ghost#000", ADMIN, PRIVATE, "private")[0].contains("No account"));
        assert!(env.svc.db.find_player("Ghost", "000", true).unwrap().is_none());

        env.riot.set_account("alice", "euw", account("Alice", "EUW", "puuid-alice", Some(30)));
        assert!(env.call("add", "Alice#EUW", ADMIN, PRIVATE, "private")[0].contains("Already tracking"));
    }

    #[test]
    fn add_when_riot_is_down_or_key_is_dead_says_so_plainly() {
        let env = env();
        env.riot.set_error(Some(RiotError::Unavailable("503".into())));
        assert!(env.call("add", "A#1", ADMIN, PRIVATE, "private")[0].contains("isn't responding"));
        env.riot.set_error(Some(RiotError::Auth("403".into())));
        assert!(env.call("add", "A#1", ADMIN, PRIVATE, "private")[0].contains("key was rejected"));
    }

    #[test]
    fn add_refuses_a_platform_where_the_account_has_no_league_profile() {
        let env = env();
        env.riot.set_account("faker", "kr1", account("Faker", "KR1", "puuid-faker", None)); // wrong platform
        let reply = env.call("add", "Faker#KR1 eun1", ADMIN, PRIVATE, "private");
        assert!(reply[0].contains("no League profile") && reply[0].contains("eun1"));
        assert!(env.svc.db.get_player("puuid-faker").unwrap().is_none()); // nothing was stored
    }

    #[test]
    fn readding_a_removed_player_does_not_announce_games_from_the_gap() {
        let env = env();
        env.call("remove", "Alice#EUW", ADMIN, PRIVATE, "private");
        env.riot.set_account("alice", "euw", account("Alice", "EUW", "puuid-alice", Some(30)));
        env.riot.set_ids("puuid-alice", &["EUW1_5"]);
        env.riot.add_match(crate::testutil::match_data("EUW1_5", T0 + 5 * MIN, 30, &["puuid-alice"]));
        env.svc.db.set_last_seen("puuid-alice", T0).unwrap(); // marker from before /remove
        env.call("add", "Alice#EUW", ADMIN, PRIVATE, "private");
        assert!(env.svc.db.open_sessions(None).unwrap().is_empty());
        assert_eq!(env.svc.db.matches_between("puuid-alice", 0, 1 << 60).unwrap().len(), 1); // stored silently
        assert_eq!(env.svc.poller.poll_once(), 0); // and not re-announced
        assert!(env.svc.db.open_sessions(None).unwrap().is_empty());
    }

    #[test]
    fn remove_soft_deletes() {
        let env = env();
        assert!(env.call("remove", "alice#euw", ADMIN, PRIVATE, "private")[0].contains("Stopped tracking"));
        assert!(env.svc.db.list_players(true).unwrap().is_empty());
        assert!(env.svc.db.find_player("Alice", "EUW", false).unwrap().is_some());
        assert!(env.call("remove", "Alice#EUW", ADMIN, PRIVATE, "private")[0].contains("isn't being tracked"));
    }

    // -- read commands ---------------------------------------------------------------

    #[test]
    fn status_reports_live_games_and_survives_riot_errors() {
        let env = env();
        env.svc.db.add_player(&make_player_full("puuid-bob", "Bob", "EUW")).unwrap();
        env.riot.set_in_game(Ok(true));
        let text = env.call("status", "", ADMIN, PRIVATE, "private").remove(0);
        assert!(text.matches("🟢").count() == 2 && text.contains("in game"));

        env.riot.set_in_game(Err(RiotError::Unavailable("down".into())));
        let text = env.call("status", "", ADMIN, PRIVATE, "private").remove(0);
        assert_eq!(text.matches("❔").count(), 2);
    }

    #[test]
    fn week_and_stats_render_from_the_database() {
        let env = env();
        let now = now_ms();
        env.svc.db.insert_match(&make_match("m1", now - 90 * MIN, 30)).unwrap();
        let mut loss = make_match("m2", now - 45 * MIN, 30);
        loss.win = 0;
        env.svc.db.insert_match(&loss).unwrap();

        let replies = env.call("week", "", ADMIN, PRIVATE, "private");
        assert!(replies[0].contains("Week of") && replies[0].contains("Alice#EUW") && replies[0].contains("2 games"));
        let stats = env.call("stats", "Alice#EUW 3", ADMIN, PRIVATE, "private").remove(0);
        assert!(stats.contains("2 games") && stats.contains("50% WR"), "{stats}");

        assert!(env.call("stats", "Alice#EUW 9999", ADMIN, PRIVATE, "private")[0].contains("between 1 and 365"));
        assert!(env.call("stats", "garbage", ADMIN, PRIVATE, "private")[0].contains("Usage"));
        assert!(env.call("stats", "Nobody#1", ADMIN, PRIVATE, "private")[0].contains("isn't being tracked"));
        assert!(env.call("week", "Nobody#1", ADMIN, PRIVATE, "private")[0].contains("isn't being tracked"));
    }

    #[test]
    fn stats_days_argument_must_be_plain_ascii_digits() {
        let env = env();
        for junk in ["²", "٣", "99999"] {
            let reply = env.call("stats", &format!("Alice#EUW {junk}"), ADMIN, PRIVATE, "private").remove(0);
            assert!(reply != REFUSAL && reply != FAILURE, "{junk}");
        }
    }

    #[test]
    fn settings_shows_effective_config_but_never_the_key() {
        let text = env().call("settings", "", ADMIN, PRIVATE, "private").remove(0);
        assert!(text.contains("session") && text.contains("Next weekly run") && text.contains("Subscribers: 0"));
        assert!(text.contains("Riot key") && !text.contains("RGAPI"));
    }

    #[test]
    fn sendweekly_broadcasts_the_summary_to_subscribers() {
        let env = env();
        assert!(env.call("sendweekly", "", ADMIN, PRIVATE, "private")[0].contains("No subscribers"));
        env.svc.db.add_subscriber(-3003, 0).unwrap();
        let replies = env.call("sendweekly", "", ADMIN, PRIVATE, "private");
        assert!(replies[0].contains("sent to 1 chat"), "{replies:?}");
        let to_group: Vec<String> = env.tg.sent().into_iter().filter(|(c, _)| *c == -3003).map(|(_, t)| t).collect();
        assert!(to_group[0].contains("Week ending"));
    }

    #[test]
    fn sendweekly_does_not_claim_success_when_nothing_was_delivered() {
        let env = env();
        env.svc.db.add_subscriber(-3003, 0).unwrap();
        env.tg.fail_chat(-3003);
        let reply = env.call("sendweekly", "", ADMIN, PRIVATE, "private").remove(0);
        assert!(!reply.contains('✅') && reply.contains("delivered"));
    }

    #[test]
    fn a_list_longer_than_telegrams_limit_arrives_in_parts() {
        let env = env();
        for i in 0..150 {
            env.svc.db.add_player(&make_player_full(&format!("p{i}"), &format!("Player{i:03}"), "EUW")).unwrap();
        }
        let replies = env.call("list", "", ADMIN, PRIVATE, "private");
        assert!(replies.len() > 1, "{}", replies.len());
        assert!(replies.iter().all(|r| crate::formatting::chars(r) <= SPLIT_AT));
        assert!(replies.concat().contains("Player149#EUW") && replies.concat().contains("Alice#EUW"));
    }

    #[test]
    fn list_shows_the_last_game_played_not_the_polling_marker() {
        let env = env();
        let now = now_ms();
        env.svc.db.set_last_seen("puuid-alice", now).unwrap(); // e.g. just added
        env.svc.db.insert_match(&make_match("m1", now - 5 * 60 * MIN, 30)).unwrap();
        let text = env.call("list", "", ADMIN, PRIVATE, "private").remove(0);
        assert!(text.contains("last game 4h ago") || text.contains("last game 5h ago"), "{text}");
    }

    // -- help / command table ----------------------------------------------------------

    #[test]
    fn every_command_is_unique_and_in_admin_help() {
        let env = env();
        let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len());
        let text = env.call("help", "", ADMIN, PRIVATE, "private").remove(0);
        for c in COMMANDS {
            assert!(text.contains(&format!("/{}", c.name)), "{}", c.name);
        }
    }

    #[test]
    fn help_hides_admin_commands_from_non_admins() {
        let env = env();
        env.svc.db.add_subscriber(GROUP, 0).unwrap();
        let text = env.call("help", "", MEMBER, GROUP, "group").remove(0);
        for c in COMMANDS {
            assert_eq!(
                text.contains(&format!("/{} ", c.name)) || text.contains(&format!("/{} —", c.name)),
                c.access == Access::Subscriber,
                "{}",
                c.name
            );
        }
        assert!(env.call("help", "", STRANGER, STRANGER, "private").is_empty());
    }

    #[test]
    fn menu_entries_satisfy_telegrams_limits_and_respect_access() {
        for entry in menu(true) {
            assert!(
                entry.command.len() <= 32
                    && entry.command.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
            assert!((1..=256).contains(&entry.description.chars().count()));
        }
        let public: Vec<String> = menu(false).into_iter().map(|e| e.command).collect();
        let subscriber: Vec<String> =
            COMMANDS.iter().filter(|c| c.access == Access::Subscriber).map(|c| c.name.to_owned()).collect();
        assert_eq!(public, subscriber);
        assert_eq!(menu(true).len(), COMMANDS.len());
    }

    #[test]
    fn publish_menu_sets_public_and_per_admin_scopes() {
        let env = env();
        publish_menu(&env.svc);
        let scopes: Vec<Option<i64>> = env.tg.menus().into_iter().map(|(scope, _)| scope).collect();
        assert_eq!(scopes, [None, Some(ADMIN)]);
    }

    #[test]
    fn ago_buckets() {
        let now = T0;
        assert_eq!(ago(None, now), "never");
        assert_eq!(ago(Some(now - 5 * MIN), now), "5m ago");
        assert_eq!(ago(Some(now - 3 * 60 * MIN), now), "3h ago");
        assert_eq!(ago(Some(now - 72 * 60 * MIN), now), "3d ago");
        assert_eq!(ago(Some(now + MIN), now), "0m ago");
    }
}
