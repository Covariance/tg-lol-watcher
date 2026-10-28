from __future__ import annotations

import pytest

from lolwatch.config import ConfigError, load_config, parse_riot_id

VALID = """
[riot]
default_platform = "kr"
poll_interval_seconds = 60

[notifications]
mode = "per_game"

[weekly]
day = "Friday"
time = "21:30"
timezone = "Europe/London"

[telegram]
admin_user_ids = [1, 2]
"""
ENV = {"RIOT_API_KEY": "RGAPI-x", "TELEGRAM_BOT_TOKEN": "123:abc"}


def write(tmp_path, text: str):
    path = tmp_path / "config.toml"
    path.write_text(text, encoding="utf8")
    return path


def test_valid_config_loads_with_env_secrets(tmp_path):
    cfg = load_config(write(tmp_path, VALID), ENV)
    assert cfg.riot.default_platform == "kr" and cfg.riot.poll_interval_seconds == 60
    assert cfg.notifications.mode == "per_game"
    assert cfg.notifications.gap_ms == 15 * 60_000          # defaults fill the gaps
    assert (cfg.weekly.day_index, cfg.weekly.time.hour, cfg.weekly.time.minute) == (5, 21, 30)
    assert cfg.telegram.admin_user_ids == (1, 2)
    assert cfg.riot_api_key == "RGAPI-x" and cfg.telegram_bot_token == "123:abc"
    assert cfg.db_path == "./lolwatch.db"


def test_db_path_comes_from_env(tmp_path):
    cfg = load_config(write(tmp_path, VALID), {**ENV, "LOLWATCH_DB": "/data/x.db"})
    assert cfg.db_path == "/data/x.db"


def test_empty_db_env_falls_back_to_default(tmp_path):
    cfg = load_config(write(tmp_path, VALID), {**ENV, "LOLWATCH_DB": ""})
    assert cfg.db_path == "./lolwatch.db"


def test_dotenv_is_loaded_but_real_environment_wins(tmp_path, monkeypatch):
    (tmp_path / ".env").write_text("RIOT_API_KEY=from-file\nTELEGRAM_BOT_TOKEN=9:file\n",
                                   encoding="utf8")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("TELEGRAM_BOT_TOKEN", raising=False)
    monkeypatch.setenv("RIOT_API_KEY", "from-env")
    cfg = load_config(write(tmp_path, VALID))
    assert cfg.riot_api_key == "from-env" and cfg.telegram_bot_token == "9:file"


def test_dotenv_parsing(tmp_path, monkeypatch):
    from lolwatch.config import load_dotenv
    (tmp_path / ".env").write_text(
        "# comment\n\nexport A_KEY=plain\nB_KEY = 'single quoted'\n"
        'C_KEY="double # not a comment"\nnot a line\nD_KEY=\n', encoding="utf8")
    for k in ("A_KEY", "B_KEY", "C_KEY", "D_KEY"):
        monkeypatch.delenv(k, raising=False)
    load_dotenv(tmp_path / ".env")
    import os
    assert os.environ["A_KEY"] == "plain" and os.environ["B_KEY"] == "single quoted"
    assert os.environ["C_KEY"] == "double # not a comment" and os.environ["D_KEY"] == ""
    load_dotenv(tmp_path / "missing.env")          # absent file is fine


def test_every_problem_is_reported_at_once(tmp_path):
    bad = """
[riot]
default_platform = "mars1"
poll_interval_seconds = 1
[notifications]
mode = "loud"
[weekly]
day = "someday"
time = "25:99"
timezone = "Mars/Olympus"
[telegram]
admin_user_ids = []
[logging]
level = "CHATTY"
"""
    with pytest.raises(ConfigError) as exc:
        load_config(write(tmp_path, bad), {})
    text = str(exc.value)
    for fragment in ("default_platform", "poll_interval_seconds", "notifications.mode",
                     "weekly.day", "weekly.time", "weekly.timezone", "admin_user_ids",
                     "logging.level", "RIOT_API_KEY", "TELEGRAM_BOT_TOKEN"):
        assert fragment in text, fragment


def test_missing_file_and_invalid_toml_are_clear_errors(tmp_path):
    with pytest.raises(ConfigError, match="not found"):
        load_config(tmp_path / "nope.toml", ENV)
    with pytest.raises(ConfigError, match="not valid TOML"):
        load_config(write(tmp_path, "[riot\n"), ENV)


def test_secrets_in_the_toml_file_are_ignored(tmp_path):
    text = VALID + '\n[secrets]\nRIOT_API_KEY = "leaked"\n'
    with pytest.raises(ConfigError, match="RIOT_API_KEY"):
        load_config(write(tmp_path, text), {"TELEGRAM_BOT_TOKEN": "x"})


def test_cli_mode_does_not_need_telegram(tmp_path):
    text = VALID.replace("admin_user_ids = [1, 2]", "admin_user_ids = []")
    cfg = load_config(write(tmp_path, text), {"RIOT_API_KEY": "k"}, need_telegram=False)
    assert cfg.riot_api_key == "k"
    with pytest.raises(ConfigError):
        load_config(write(tmp_path, text), {"RIOT_API_KEY": "k"})


def test_parse_riot_id():
    assert parse_riot_id("Faker#KR1") == ("Faker", "KR1")
    assert parse_riot_id("  Two Words #EUW ") == ("Two Words", "EUW")
    assert parse_riot_id("Has#Hash#TAG") == ("Has#Hash", "TAG")   # split on the last '#'
    for bad in ("", "nohash", "#TAG", "Name#"):
        with pytest.raises(ValueError):
            parse_riot_id(bad)
