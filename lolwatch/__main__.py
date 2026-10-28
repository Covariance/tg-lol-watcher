"""Entrypoint: `uv run python -m lolwatch`."""

from __future__ import annotations

import logging
import sys

from .config import ConfigError, load_config


def setup_logging(level: str) -> None:
    # Player names are often CJK; a cp1252 Windows console would raise on every such line.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    logging.basicConfig(
        level=level, stream=sys.stdout,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    # httpx logs full request URLs at INFO, and Telegram's contain the bot token.
    # Riot URLs carry PUUIDs too.
    for noisy in ("httpx", "httpcore", "apscheduler"):
        logging.getLogger(noisy).setLevel(logging.WARNING)


def main() -> int:
    try:
        cfg = load_config()
    except ConfigError as exc:
        print(f"lolwatch: invalid configuration:\n{exc}", file=sys.stderr)
        return 2
    setup_logging(cfg.log_level)

    from .app import StartupError, run  # imported late: needs a valid config first

    try:
        run(cfg)
    except StartupError as exc:
        logging.getLogger("lolwatch").critical("refusing to start: %s", exc)
        return 1
    except Exception:
        logging.getLogger("lolwatch").critical("fatal error", exc_info=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
