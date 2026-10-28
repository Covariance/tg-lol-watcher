//! Pure match-v5 `info` -> MatchData parsing. No I/O.
//!
//! This is where the known traps live (duration units, remakes, Arena), so it is
//! kept separate from the transport and tested against fixture JSON. Deserialising
//! straight into these borrowed structs skips every field we don't use (a match is
//! ~30 KB of JSON; we keep a few dozen bytes per participant).

use std::collections::HashMap;

use serde::Deserialize;

use crate::models::{LOSS, MatchData, ParticipantResult, UNKNOWN, WIN, is_no_win_loss_queue};

/// Longer than this is a parsing bug, not a long game.
pub const MAX_GAME_S: i64 = 4 * 3600;
const REMAKE_UNDER_S: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchParseError(pub String);

impl std::fmt::Display for MatchParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A whole match-v5 response; only `info` matters.
#[derive(Deserialize)]
pub struct MatchDto {
    pub info: Option<Info>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub game_creation: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub game_start_timestamp: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub game_end_timestamp: Option<i64>,
    #[serde(default)]
    pub game_duration: Option<f64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub queue_id: Option<i64>,
    #[serde(default)]
    pub participants: Option<Vec<Participant>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    pub puuid: Option<String>,
    pub champion_name: Option<String>,
    pub champion_id: Option<i64>,
    #[serde(default, deserialize_with = "strict_bool")]
    pub win: Option<bool>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub kills: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub deaths: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub assists: Option<i64>,
    #[serde(default, deserialize_with = "strict_bool")]
    pub game_ended_in_early_surrender: Option<bool>,
}

/// An integer, or None for null / any other JSON type (never a hard error: one odd
/// field must not make a whole match unparseable).
fn lenient_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_i64(),
        _ => None,
    })
}

fn strict_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(serde_json::Value::deserialize(d)?.as_bool())
}

fn sane(seconds: f64) -> bool {
    0.0 < seconds && seconds <= MAX_GAME_S as f64
}

/// Python's round(): half to even. Durations are whole seconds in practice.
fn round_half_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 { (r - x.signum()) as i64 } else { r as i64 }
}

/// (start_ms, end_ms, duration_s).
///
/// Prefers gameEndTimestamp - gameStartTimestamp. `gameDuration` is seconds on patch
/// 11.20+ but milliseconds on older matches, so it is only a fallback and is
/// unit-corrected.
pub fn derive_times(info: &Info) -> Result<(i64, i64, i64), MatchParseError> {
    let start = info
        .game_start_timestamp
        .filter(|&t| t != 0)
        .or(info.game_creation.filter(|&t| t != 0))
        .ok_or_else(|| MatchParseError("match has no gameStartTimestamp/gameCreation".into()))?;

    if let (Some(end), Some(_)) =
        (info.game_end_timestamp.filter(|&t| t != 0), info.game_start_timestamp.filter(|&t| t != 0))
    {
        let seconds = (end - start) as f64 / 1000.0;
        if sane(seconds) {
            return Ok((start, end, round_half_even(seconds)));
        }
    }

    if let Some(raw) = info.game_duration.filter(|&d| d != 0.0) {
        let mut seconds = raw;
        if !sane(seconds) {
            seconds /= 1000.0; // pre-11.20 millisecond durations
        }
        if sane(seconds) {
            return Ok((start, start + round_half_even(seconds * 1000.0), round_half_even(seconds)));
        }
    }

    Err(MatchParseError(format!(
        "cannot derive a sane duration (start={start}, end={:?}, gameDuration={:?})",
        info.game_end_timestamp, info.game_duration
    )))
}

/// Data Dragon ids that differ from the in-game display name.
fn champion(p: &Participant) -> String {
    match p.champion_name.as_deref() {
        Some("MonkeyKing") => "Wukong".into(),
        Some("FiddleSticks") => "Fiddlesticks".into(),
        Some(name) if !name.is_empty() => name.into(),
        _ => match p.champion_id {
            Some(id) => format!("Champion {id}"),
            None => "Unknown".into(),
        },
    }
}

fn result(queue_id: i64, p: &Participant) -> i64 {
    // Arena and other placement modes: `win` doesn't mean what it means on the Rift.
    if is_no_win_loss_queue(queue_id) {
        return UNKNOWN;
    }
    match p.win {
        Some(true) => WIN,
        Some(false) => LOSS,
        None => UNKNOWN,
    }
}

/// Parse the `info` object of a match-v5 response.
pub fn parse_match(match_id: &str, info: Info) -> Result<MatchData, MatchParseError> {
    let (start_ms, end_ms, duration_s) = derive_times(&info)?;
    let queue_id = info.queue_id.unwrap_or(0);

    let mut participants = HashMap::new();
    let mut early_surrender = false;
    for p in info.participants.unwrap_or_default() {
        early_surrender |= p.game_ended_in_early_surrender.unwrap_or(false);
        let Some(puuid) = p.puuid.as_deref().filter(|id| !id.is_empty() && *id != "BOT") else {
            continue;
        };
        let parsed = ParticipantResult {
            champion: champion(&p),
            win: result(queue_id, &p),
            kills: p.kills,
            deaths: p.deaths,
            assists: p.assists,
        };
        participants.insert(puuid.to_owned(), parsed);
    }

    Ok(MatchData {
        match_id: match_id.to_owned(),
        queue_id,
        duration_s,
        start_ms,
        end_ms,
        remake: early_surrender || duration_s < REMAKE_UNDER_S,
        participants,
    })
}

/// Parse a full match-v5 response body.
pub fn parse_match_json(match_id: &str, body: &[u8]) -> Result<MatchData, MatchParseError> {
    let dto: MatchDto = serde_json::from_slice(body)
        .map_err(|e| MatchParseError(format!("match {match_id} is not match-v5 JSON: {e}")))?;
    let info = dto.info.ok_or_else(|| MatchParseError(format!("match {match_id} has no info object")))?;
    parse_match(match_id, info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fixture;

    fn parsed(name: &str) -> Result<MatchData, MatchParseError> {
        let raw = fixture(name);
        let id = serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["metadata"]["matchId"]
            .as_str()
            .unwrap()
            .to_owned();
        parse_match_json(&id, &raw)
    }

    fn info_of(name: &str) -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(&fixture(name)).unwrap()["info"].clone()
    }

    fn parse_value(v: serde_json::Value) -> Result<MatchData, MatchParseError> {
        parse_match("x", serde_json::from_value(v).unwrap())
    }

    #[test]
    fn ranked_match_uses_end_minus_start_and_maps_participants() {
        let m = parsed("match_ranked").unwrap();
        assert_eq!((m.queue_id, m.duration_s, m.remake), (420, 1860, false));
        assert_eq!(m.end_ms - m.start_ms, 1_860_000);
        let alice = &m.participants["puuid-alice"];
        assert_eq!(
            (alice.champion.as_str(), alice.win, alice.kills, alice.deaths, alice.assists),
            ("Azir", WIN, Some(8), Some(2), Some(11))
        );
        assert_eq!(m.participants["puuid-bob"].champion, "Wukong"); // MonkeyKing display name
        assert_eq!(m.participants["puuid-x1"].win, LOSS); // opponents are parsed too
        assert_eq!(m.participants.len(), 3);
    }

    #[test]
    fn pre_patch_11_20_millisecond_duration_is_corrected() {
        let m = parsed("match_old_ms").unwrap();
        assert_eq!(m.duration_s, 1500); // 1_500_000 "ms" -> 25 minutes
        assert_eq!(m.end_ms, m.start_ms + 1_500_000);
    }

    #[test]
    fn remake_is_flagged_by_early_surrender_and_short_duration() {
        assert!(parsed("match_remake").unwrap().remake);
        let mut raw = info_of("match_remake");
        raw["participants"][0]["gameEndedInEarlySurrender"] = false.into();
        assert!(parse_value(raw).unwrap().remake); // 200s is under 5 minutes anyway
    }

    #[test]
    fn arena_result_is_unknown_rather_than_a_misleading_win() {
        let m = parsed("match_arena").unwrap();
        assert_eq!(m.queue_id, 1700);
        assert_eq!(m.participants["puuid-alice"].win, UNKNOWN);
        assert!(!m.remake);
    }

    #[test]
    fn bot_participants_are_skipped() {
        let m = parsed("match_bots").unwrap();
        assert_eq!(m.participants.keys().collect::<Vec<_>>(), ["puuid-alice"]);
    }

    #[test]
    fn absurd_duration_is_a_parsing_error_not_a_long_game() {
        assert!(parsed("match_absurd").is_err());
    }

    #[test]
    fn missing_timestamps_raise() {
        assert!(parse_value(serde_json::json!({"queueId": 420, "participants": []})).is_err());
    }

    #[test]
    fn missing_info_is_a_parse_error() {
        assert!(parse_match_json("x", br#"{"metadata": {}}"#).is_err());
        assert!(parse_match_json("x", b"not json").is_err());
    }

    #[test]
    fn loss_and_missing_stats_are_tolerated() {
        let mut info = info_of("match_ranked");
        info["participants"][0]["win"] = false.into();
        info["participants"][0]["kills"] = serde_json::Value::Null;
        info["participants"][0].as_object_mut().unwrap().remove("assists");
        let m = parse_value(info).unwrap();
        let p = &m.participants["puuid-alice"];
        assert_eq!((p.win, p.kills, p.assists), (LOSS, None, None));
    }

    #[test]
    fn derive_times_falls_back_to_duration_when_end_is_insane() {
        let info: Info = serde_json::from_value(serde_json::json!({
            "gameStartTimestamp": 1_000_000,
            "gameEndTimestamp": 1_000_000 + 999_999_999,
            "gameDuration": 600,
        }))
        .unwrap();
        assert_eq!(derive_times(&info).unwrap(), (1_000_000, 1_600_000, 600));
    }
}
