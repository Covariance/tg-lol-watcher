"""TOML + environment config, validated as a whole at startup."""

from __future__ import annotations

import os
import re
import tomllib
from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import time
from pathlib import Path
from typing import Any
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from dotenv import load_dotenv

from .riot.routing import PLATFORMS

MODES = ("session", "per_game", "off")
# PTB run_daily numbers days 0=Sunday .. 6=Saturday.
DAYS = {"sunday": 0, "monday": 1, "tuesday": 2, "wednesday": 3, "thursday": 4,
        "friday": 5, "saturday": 6}
LOG_LEVELS = ("DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL")


class ConfigError(Exception):
    """Raised with every problem found, one per line."""


@dataclass(frozen=True)
class RiotConfig:
    default_platform: str = "euw1"
    poll_interval_seconds: int = 120


@dataclass(frozen=True)
class NotificationConfig:
    mode: str = "session"
    session_gap_minutes: int = 15
    flush_recheck_minutes: int = 5
    max_postpones: int = 18

    @property
    def gap_ms(self) -> int:
        return self.session_gap_minutes * 60_000

    @property
    def recheck_ms(self) -> int:
        return self.flush_recheck_minutes * 60_000


@dataclass(frozen=True)
class WeeklyConfig:
    enabled: bool = True
    day: str = "sunday"
    time: time = time(20, 0)
    timezone: str = "Europe/London"

    @property
    def tz(self) -> ZoneInfo:
        return ZoneInfo(self.timezone)

    @property
    def day_index(self) -> int:
        return DAYS[self.day]


@dataclass(frozen=True)
class TelegramConfig:
    admin_user_ids: tuple[int, ...] = ()


@dataclass(frozen=True)
class Config:
    riot: RiotConfig = field(default_factory=RiotConfig)
    notifications: NotificationConfig = field(default_factory=NotificationConfig)
    weekly: WeeklyConfig = field(default_factory=WeeklyConfig)
    telegram: TelegramConfig = field(default_factory=TelegramConfig)
    log_level: str = "INFO"
    # secrets / env
    riot_api_key: str = ""
    telegram_bot_token: str = ""
    db_path: str = "./lolwatch.db"

    @property
    def tz(self) -> ZoneInfo:
        """Timezone used for rendering. Same as the weekly schedule's."""
        return self.weekly.tz


def parse_riot_id(text: str) -> tuple[str, str]:
    """`GameName#TAG` -> (game_name, tag_line). Raises ValueError."""
    name, sep, tag = text.strip().rpartition("#")
    name, tag = name.strip(), tag.strip()
    if not sep or not name or not tag:
        raise ValueError(f"expected GameName#TAG, got {text!r}")
    return name, tag


def _int(errors: list[str], where: str, value: Any, minimum: int = 1) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        errors.append(f"{where}: expected an integer >= {minimum}, got {value!r}")
        return minimum
    return value


def _section(errors: list[str], raw: Mapping[str, Any], name: str) -> Mapping[str, Any]:
    value = raw.get(name, {})
    if not isinstance(value, dict):
        errors.append(f"[{name}] must be a table")
        return {}
    return value


def load_config(path: str | Path | None = None,
                env: Mapping[str, str] | None = None, *,
                need_telegram: bool = True) -> Config:
    """Load and validate. Raises ConfigError listing *all* problems.

    `need_telegram=False` (CLI subcommands) skips the Telegram token and admin list.
    When `env` is omitted, `./.env` is loaded first; real environment variables win.
    """
    if env is None:
        load_dotenv(".env", override=False)
        env = os.environ
    path = Path(path or env.get("LOLWATCH_CONFIG") or "config.toml")
    errors: list[str] = []

    raw: dict[str, Any] = {}
    try:
        with open(path, "rb") as fh:
            raw = tomllib.load(fh)
    except FileNotFoundError:
        errors.append(f"config file not found: {path} (copy config.example.toml)")
    except tomllib.TOMLDecodeError as exc:
        errors.append(f"config file {path} is not valid TOML: {exc}")

    riot_raw = _section(errors, raw, "riot")
    default_platform = str(riot_raw.get("default_platform", "euw1")).lower()
    if default_platform not in PLATFORMS:
        errors.append(f"riot.default_platform: unknown platform {default_platform!r}")
    poll = _int(errors, "riot.poll_interval_seconds",
                riot_raw.get("poll_interval_seconds", 120), minimum=10)

    n_raw = _section(errors, raw, "notifications")
    mode = n_raw.get("mode", "session")
    if mode not in MODES:
        errors.append(f"notifications.mode: expected one of {MODES}, got {mode!r}")
    notifications = NotificationConfig(
        mode=mode,
        session_gap_minutes=_int(errors, "notifications.session_gap_minutes",
                                 n_raw.get("session_gap_minutes", 15)),
        flush_recheck_minutes=_int(errors, "notifications.flush_recheck_minutes",
                                   n_raw.get("flush_recheck_minutes", 5)),
        max_postpones=_int(errors, "notifications.max_postpones",
                           n_raw.get("max_postpones", 18), minimum=0),
    )

    w_raw = _section(errors, raw, "weekly")
    day = str(w_raw.get("day", "sunday")).lower()
    if day not in DAYS:
        errors.append(f"weekly.day: expected a weekday name, got {day!r}")
        day = "sunday"
    time_text = str(w_raw.get("time", "20:00"))
    weekly_time = time(20, 0)
    m = re.fullmatch(r"(\d{1,2}):(\d{2})", time_text)
    if m and int(m[1]) < 24 and int(m[2]) < 60:
        weekly_time = time(int(m[1]), int(m[2]))
    else:
        errors.append(f"weekly.time: expected HH:MM, got {time_text!r}")
    tz_name = str(w_raw.get("timezone", "Europe/London"))
    try:
        ZoneInfo(tz_name)
    except (ZoneInfoNotFoundError, ValueError):
        errors.append(f"weekly.timezone: unknown timezone {tz_name!r}")
    enabled = w_raw.get("enabled", True)
    if not isinstance(enabled, bool):
        errors.append(f"weekly.enabled: expected true/false, got {enabled!r}")
        enabled = True
    weekly = WeeklyConfig(enabled=enabled, day=day, time=weekly_time, timezone=tz_name)

    t_raw = _section(errors, raw, "telegram")
    admins = t_raw.get("admin_user_ids", [])
    if (not isinstance(admins, list) or (need_telegram and not admins)
            or not all(isinstance(a, int) and not isinstance(a, bool) for a in admins)):
        errors.append("telegram.admin_user_ids: expected a non-empty list of integer user ids")
        admins = []

    level = str(_section(errors, raw, "logging").get("level", "INFO")).upper()
    if level not in LOG_LEVELS:
        errors.append(f"logging.level: expected one of {LOG_LEVELS}, got {level!r}")
        level = "INFO"

    riot_key = env.get("RIOT_API_KEY", "").strip()
    tg_token = env.get("TELEGRAM_BOT_TOKEN", "").strip()
    if not riot_key:
        errors.append("environment: RIOT_API_KEY is not set")
    if need_telegram and not tg_token:
        errors.append("environment: TELEGRAM_BOT_TOKEN is not set")

    if errors:
        raise ConfigError("\n".join(errors))

    return Config(
        riot=RiotConfig(default_platform=default_platform, poll_interval_seconds=poll),
        notifications=notifications,
        weekly=weekly,
        telegram=TelegramConfig(admin_user_ids=tuple(admins)),
        log_level=level,
        riot_api_key=riot_key,
        telegram_bot_token=tg_token,
        db_path=env.get("LOLWATCH_DB") or "./lolwatch.db",
    )
