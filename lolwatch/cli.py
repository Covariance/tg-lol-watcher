"""Operator CLI: `uv run python -m lolwatch.cli <command>`.

resolve         look up a Riot ID end to end (key + account-v1)
backfill        silently store the last N days of history for every player
repuuid         re-resolve every PUUID after an API key change (spec §4.4)
dry-run-summary print the weekly summary to the terminal; nothing is sent
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import re
import sys
from datetime import datetime

from .app import KEY_FP
from .config import Config, ConfigError, load_config, parse_riot_id
from .db import Database, key_fingerprint
from .poller import Poller
from .reports import render_window
from .riot.adapter import RiotClient
from .riot.cassiopeia_impl import CassiopeiaClient
from .riot.errors import RiotAuthError, RiotError, RiotNotFound
from .riot.retry import RetryingClient
from .riot.routing import PLATFORMS

log = logging.getLogger("lolwatch.cli")


def _riot(cfg: Config) -> RiotClient:
    return RetryingClient(CassiopeiaClient(cfg.riot_api_key))


async def cmd_resolve(cfg: Config, args: argparse.Namespace) -> int:
    name, tag = parse_riot_id(args.riot_id)
    platform = (args.platform or cfg.riot.default_platform).lower()
    try:
        acct = await _riot(cfg).resolve_account(name, tag, platform)
    except RiotNotFound:
        print(f"not found: {name}#{tag} on {platform}", file=sys.stderr)
        return 1
    print(f"{acct.game_name}#{acct.tag_line}  platform={platform}  "
          f"level={acct.summoner_level}\npuuid={acct.puuid}")
    return 0


async def cmd_backfill(cfg: Config, args: argparse.Namespace) -> int:
    db = await Database.open(cfg.db_path)
    try:
        poller = Poller(db, _riot(cfg), sessions=None, alerter=None)
        players = await db.list_players()
        if not players:
            print("no players in the database (add one with /add in Telegram)")
        for p in players:
            n = await poller.backfill(p, args.days)
            print(f"{p.riot_id}: {n} new match(es) stored")
    finally:
        await db.close()
    return 0


async def cmd_repuuid(cfg: Config, args: argparse.Namespace) -> int:
    """Re-resolve every player's PUUID under the *current* key and cascade it."""
    db = await Database.open(cfg.db_path)
    riot = _riot(cfg)
    failures = 0
    try:
        for p in await db.list_players(active_only=False):
            try:
                acct = await riot.resolve_account(p.game_name, p.tag_line, p.platform)
            except RiotNotFound:
                print(f"{p.riot_id}: NOT FOUND (renamed?); left unchanged", file=sys.stderr)
                failures += 1
                continue
            if acct.puuid == p.puuid:
                print(f"{p.riot_id}: unchanged")
                continue
            if await db.get_player(acct.puuid):
                print(f"{p.riot_id}: new PUUID already exists in the DB; skipping",
                      file=sys.stderr)
                failures += 1
                continue
            await db.rewrite_puuid(p.puuid, acct.puuid)
            print(f"{p.riot_id}: PUUID rewritten")
        if failures == 0:
            await db.kv_set(KEY_FP, key_fingerprint(cfg.riot_api_key))
            print("done; key fingerprint recorded (start-up warning cleared)")
        else:
            print(f"{failures} player(s) need attention; fingerprint NOT updated",
                  file=sys.stderr)
    except RiotAuthError as exc:
        print(f"Riot rejected the key: {exc}", file=sys.stderr)
        return 1
    finally:
        await db.close()
    return 1 if failures else 0


async def cmd_dry_run_summary(cfg: Config, args: argparse.Namespace) -> int:
    db = await Database.open(cfg.db_path)
    try:
        players = await db.list_players()
        for text in await render_window(db, cfg, players, days=args.days,
                                        now=datetime.now(cfg.tz)):
            print(re.sub(r"</?(b|pre)>", "", text), end="\n\n")
    finally:
        await db.close()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="lolwatch.cli", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("resolve", help="look up a Riot ID")
    p.add_argument("riot_id", help="GameName#TAG")
    p.add_argument("--platform", choices=sorted(PLATFORMS))
    p.set_defaults(fn=cmd_resolve)

    p = sub.add_parser("backfill", help="store recent history silently")
    p.add_argument("--days", type=int, default=14)
    p.set_defaults(fn=cmd_backfill)

    p = sub.add_parser("repuuid", help="re-resolve PUUIDs after an API key change")
    p.set_defaults(fn=cmd_repuuid)

    p = sub.add_parser("dry-run-summary", help="print the weekly summary; sends nothing")
    p.add_argument("--days", type=int, default=7)
    p.set_defaults(fn=cmd_dry_run_summary)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        cfg = load_config(need_telegram=False)
    except ConfigError as exc:
        print(f"lolwatch: invalid configuration:\n{exc}", file=sys.stderr)
        return 2
    logging.basicConfig(level=cfg.log_level, stream=sys.stderr,
                        format="%(levelname)s %(name)s: %(message)s")
    try:
        return asyncio.run(args.fn(cfg, args))
    except RiotAuthError as exc:
        print(f"Riot rejected the API key (403): {exc}", file=sys.stderr)
        return 1
    except RiotError as exc:
        print(f"Riot error: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
