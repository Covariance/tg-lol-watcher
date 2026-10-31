//! The slice of the Telegram Bot API the bot uses, over plain HTTPS + JSON.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::http::Http;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl Chat {
    pub fn is_private(&self) -> bool {
        self.kind == "private"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entity {
    #[serde(rename = "type")]
    pub kind: String,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Message {
    #[serde(default)]
    pub from: Option<User>,
    /// Set when the message was sent on behalf of a chat (anonymous admins, channels).
    #[serde(default)]
    pub sender_chat: Option<Chat>,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
    /// Service message in a new supergroup: the group it was upgraded from.
    #[serde(default)]
    pub migrate_from_chat_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    /// Only new messages are requested (`allowed_updates`); edits never arrive.
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TgError {
    /// Flood control: wait this many seconds.
    RetryAfter(f64),
    /// 403: the bot was blocked or removed from the chat.
    Forbidden(String),
    /// The group became a supergroup; it now lives at this chat id.
    Migrated(i64),
    Other(String),
}

impl fmt::Display for TgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetryAfter(s) => write!(f, "flood control: retry after {s}s"),
            Self::Migrated(to) => write!(f, "group chat was upgraded to supergroup {to}"),
            Self::Forbidden(m) | Self::Other(m) => f.write_str(m),
        }
    }
}

pub type TgResult<T> = Result<T, TgError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
}

pub trait Telegram: Send + Sync {
    fn get_me(&self) -> TgResult<User>;
    /// Long poll. Returns the updates after `offset`.
    fn get_updates(&self, offset: i64, timeout_s: u32) -> TgResult<Vec<Update>>;
    /// HTML when `html`, plain text otherwise. Link previews are always off.
    fn send_message(&self, chat_id: i64, text: &str, html: bool) -> TgResult<()>;
    /// `chat_id = None` is the default scope (everyone).
    fn set_my_commands(&self, commands: &[BotCommand], chat_id: Option<i64>) -> TgResult<()>;
    /// Forget updates that queued while the bot was down.
    fn drop_pending_updates(&self) -> TgResult<()>;
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<u16>,
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    retry_after: Option<f64>,
    migrate_to_chat_id: Option<i64>,
}

pub struct BotApi {
    http: Arc<dyn Http>,
    base: String,
}

impl BotApi {
    pub fn new(token: &str, http: Arc<dyn Http>) -> Self {
        Self::with_base_url(token, http, None)
    }

    /// `base` replaces `https://api.telegram.org` (a test hook, or a local Bot API server).
    pub fn with_base_url(token: &str, http: Arc<dyn Http>, base: Option<&str>) -> Self {
        let base = base.unwrap_or("https://api.telegram.org").trim_end_matches('/');
        Self { http, base: format!("{base}/bot{token}/") }
    }

    fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: serde_json::Value) -> TgResult<T> {
        let payload = serde_json::to_vec(&body).expect("JSON values always serialise");
        // Never put the URL in an error: it carries the bot token.
        let response = self
            .http
            .post_json(&format!("{}{method}", self.base), &payload)
            .map_err(|e| TgError::Other(format!("{method}: transport: {}", e.0.replace(&self.base, "…/"))))?;
        let parsed: ApiResponse<T> = serde_json::from_slice(&response.body)
            .map_err(|e| TgError::Other(format!("{method}: HTTP {}: unreadable reply ({e})", response.status)))?;
        if parsed.ok {
            return parsed.result.ok_or_else(|| TgError::Other(format!("{method}: ok without a result")));
        }
        let description = parsed.description.unwrap_or_default();
        let (retry_after, migrated) = parsed.parameters.map_or((None, None), |p| (p.retry_after, p.migrate_to_chat_id));
        match (parsed.error_code.unwrap_or(response.status), retry_after, migrated) {
            (_, _, Some(to)) => Err(TgError::Migrated(to)),
            (_, Some(seconds), _) => {
                Err(TgError::RetryAfter(if seconds.is_finite() { seconds.clamp(0.0, 300.0) } else { 5.0 }))
            }
            (403, _, _) => Err(TgError::Forbidden(description)),
            (code, _, _) => Err(TgError::Other(format!("{method}: {code} {description}"))),
        }
    }
}

impl Telegram for BotApi {
    fn get_me(&self) -> TgResult<User> {
        self.call("getMe", serde_json::json!({}))
    }

    fn get_updates(&self, offset: i64, timeout_s: u32) -> TgResult<Vec<Update>> {
        let raw: Vec<serde_json::Value> = self.call(
            "getUpdates",
            serde_json::json!({"offset": offset, "timeout": timeout_s, "allowed_updates": ["message"]}),
        )?;
        Ok(raw.into_iter().filter_map(lenient_update).collect())
    }

    fn send_message(&self, chat_id: i64, text: &str, html: bool) -> TgResult<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "link_preview_options": {"is_disabled": true},
        });
        if html {
            body["parse_mode"] = "HTML".into();
        }
        self.call::<serde::de::IgnoredAny>("sendMessage", body).map(drop)
    }

    fn set_my_commands(&self, commands: &[BotCommand], chat_id: Option<i64>) -> TgResult<()> {
        let mut body = serde_json::json!({"commands": commands});
        if let Some(chat_id) = chat_id {
            body["scope"] = serde_json::json!({"type": "chat", "chat_id": chat_id});
        }
        self.call::<bool>("setMyCommands", body).map(drop)
    }

    fn drop_pending_updates(&self) -> TgResult<()> {
        self.call::<bool>("deleteWebhook", serde_json::json!({"drop_pending_updates": true})).map(drop)
    }
}

/// One update, or an empty one with the same id when its shape is unexpected: one
/// odd update must neither fail the batch nor stall the offset on it forever.
fn lenient_update(value: serde_json::Value) -> Option<Update> {
    let update_id = value.get("update_id")?.as_i64()?;
    Some(Update::deserialize(&value).unwrap_or_else(|e| {
        log::warn!("telegram: skipping update {update_id} of an unexpected shape: {e}");
        Update { update_id, message: None }
    }))
}

/// A parsed bot command: `/name@bot arg1 arg2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
}

/// Like python-telegram-bot's CommandHandler: the message must *start* with a
/// bot_command entity; `/cmd@otherbot` is not for us; names are case-insensitive.
pub fn parse_command(message: &Message, bot_username: &str) -> Option<Command> {
    let text = message.text.as_deref()?;
    let entity = message.entities.first().filter(|e| e.kind == "bot_command" && e.offset == 0)?;
    // Offsets are UTF-16 units; a command is ASCII, so units == bytes here.
    let head = text.get(..entity.length)?;
    let head = head.strip_prefix('/')?;
    let (name, target) = match head.split_once('@') {
        Some((name, target)) => (name, Some(target)),
        None => (head, None),
    };
    if target.is_some_and(|t| !t.eq_ignore_ascii_case(bot_username)) {
        return None;
    }
    let args = text[entity.length..].split_whitespace().map(str::to_owned).collect();
    Some(Command { name: name.to_ascii_lowercase(), args })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Response;
    use crate::testutil::FakeHttp;

    fn message(text: &str, command_len: usize) -> Message {
        Message {
            text: Some(text.into()),
            entities: vec![Entity { kind: "bot_command".into(), offset: 0, length: command_len }],
            ..Default::default()
        }
    }

    #[test]
    fn commands_parse_like_python_telegram_bot() {
        let c = parse_command(&message("/Add Some Name#EUW  kr", 4), "tb").unwrap();
        assert_eq!((c.name.as_str(), c.args), ("add", vec!["Some".to_owned(), "Name#EUW".into(), "kr".into()]));
        assert_eq!(parse_command(&message("/list@tb", 8), "TB").unwrap().name, "list");
        assert_eq!(parse_command(&message("/list@otherbot", 14), "tb"), None);
        let mut plain = message("hello", 0);
        plain.entities.clear();
        assert_eq!(parse_command(&plain, "tb"), None);
        let mut mid = message("see /list", 5);
        mid.entities[0].offset = 4;
        assert_eq!(parse_command(&mid, "tb"), None); // must start the message
    }

    #[test]
    fn updates_deserialize_and_ignore_unknown_fields() {
        let json = r#"[{"update_id": 7, "message": {"message_id": 1, "date": 0,
            "from": {"id": 5, "is_bot": false, "first_name": "A"},
            "chat": {"id": 5, "type": "private"}, "text": "/help",
            "entities": [{"type": "bot_command", "offset": 0, "length": 5}]}},
            {"update_id": 8, "edited_message": {}}]"#;
        let updates: Vec<Update> = serde_json::from_str(json).unwrap();
        assert_eq!(updates.len(), 2);
        let m = updates[0].message.as_ref().unwrap();
        assert_eq!((m.from.as_ref().unwrap().id, m.chat.kind.as_str()), (5, "private"));
        assert!(updates[1].message.is_none());
    }

    #[test]
    fn an_odd_update_is_skipped_without_failing_the_batch() {
        let http = Arc::new(FakeHttp::default());
        let body = r#"{"ok": true, "result": [
            {"update_id": 1, "message": {"chat": "not an object"}},
            {"update_id": 2, "message": {"chat": {"id": 5, "type": "private"}, "text": "hi"}}]}"#;
        http.route_full(
            "api.telegram.org/bot1:x/getUpdates",
            Response { status: 200, retry_after: None, rate_limit_type: None, body: body.as_bytes().to_vec() },
        );
        let updates = BotApi::new("1:x", http).get_updates(0, 0).unwrap();
        assert_eq!(updates.iter().map(|u| u.update_id).collect::<Vec<_>>(), [1, 2]);
        assert!(updates[0].message.is_none() && updates[1].message.is_some());
    }

    fn api_returning(status: u16, body: &str) -> (BotApi, Arc<FakeHttp>) {
        let http = Arc::new(FakeHttp::default());
        http.route_full(
            "api.telegram.org/bot1:x/sendMessage",
            Response { status, retry_after: None, rate_limit_type: None, body: body.as_bytes().to_vec() },
        );
        (BotApi::new("1:x", http.clone()), http)
    }

    #[test]
    fn send_message_maps_flood_control_and_blocks() {
        let (api, http) = api_returning(200, r#"{"ok": true, "result": {"message_id": 1}}"#);
        assert_eq!(api.send_message(5, "<b>x</b>", true), Ok(()));
        let sent: serde_json::Value = serde_json::from_slice(&http.posts()[0].1).unwrap();
        assert_eq!((sent["chat_id"].as_i64(), sent["parse_mode"].as_str()), (Some(5), Some("HTML")));
        assert_eq!(sent["link_preview_options"]["is_disabled"], true);

        let (api, _) = api_returning(
            429,
            r#"{"ok": false, "error_code": 429, "description": "Too Many Requests", "parameters": {"retry_after": 3}}"#,
        );
        assert_eq!(api.send_message(5, "x", false), Err(TgError::RetryAfter(3.0)));
        let (api, _) = api_returning(
            403,
            r#"{"ok": false, "error_code": 403, "description": "Forbidden: bot was blocked by the user"}"#,
        );
        assert!(matches!(api.send_message(5, "x", false), Err(TgError::Forbidden(_))));
        let (api, _) = api_returning(502, "<html>bad gateway</html>");
        assert!(matches!(api.send_message(5, "x", false), Err(TgError::Other(_))));
    }

    #[test]
    fn a_group_upgrade_reports_the_new_chat_id() {
        let (api, _) = api_returning(
            400,
            r#"{"ok": false, "error_code": 400, "description": "Bad Request: group chat was upgraded to a supergroup chat",
                "parameters": {"migrate_to_chat_id": -1001234}}"#,
        );
        assert_eq!(api.send_message(-55, "x", true), Err(TgError::Migrated(-1001234)));
    }

    #[test]
    fn transport_errors_never_leak_the_token() {
        let http = Arc::new(FakeHttp::default());
        http.fail_with(crate::http::TransportError("dns error for https://api.telegram.org/botSECRET:t/getMe".into()));
        let api = BotApi::new("SECRET:t", http);
        let err = api.get_me().unwrap_err().to_string();
        assert!(!err.contains("SECRET"), "{err}");
    }
}
