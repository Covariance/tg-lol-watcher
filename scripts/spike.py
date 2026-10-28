"""Live smoke test — check the Riot client against the LIVE API before trusting the app.

    RIOT_API_KEY=RGAPI-... uv run python scripts/spike.py "GameName#TAG" [platform]

Runs the app's own `HttpRiotClient` (no retries), in order:
  a) account-v1 lookup by Riot ID (+ summoner level)
  b) match-v5 history + one full match parse
  c) spectator-v5 live-game lookup by PUUID
Throwaway: not imported by the app.
"""

from __future__ import annotations

import asyncio
import os
import sys
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))   # run from anywhere
from lolwatch.riot.http_client import HttpRiotClient  # noqa: E402


def step(label: str):
    print(f"\n=== {label}")


async def run(key: str, name: str, tag: str, platform: str) -> int:
    riot = HttpRiotClient(key)
    results: dict[str, bool] = {}
    try:
        step("a) account by Riot ID")
        try:
            acct = await riot.resolve_account(name, tag, platform)
            print(f"OK  {acct.game_name}#{acct.tag_line}  level={acct.summoner_level}  "
                  f"puuid={acct.puuid[:12]}…")
            results["a"] = True
        except Exception:
            traceback.print_exc()
            print("FAIL: cannot continue without a PUUID (403 => key expired/invalid)")
            return 1

        step("b) match-v5 history + full match parse")
        try:
            ids = await riot.recent_match_ids(acct.puuid, platform, None, 5)
            print(f"ids: {ids}")
            parsed = await riot.get_match(ids[0], platform)
            me = parsed.participants.get(acct.puuid)
            print(f"OK  queue={parsed.queue_id} duration={parsed.duration_s}s "
                  f"remake={parsed.remake} me={me}")
            results["b"] = me is not None
        except Exception:
            traceback.print_exc()
            results["b"] = False

        step("c) spectator-v5 by PUUID")
        try:
            in_game = await riot.is_in_game(acct.puuid, platform)
            print(f"OK  in_game={in_game}  (False is the normal answer when idle)")
            results["c"] = True
        except Exception:
            traceback.print_exc()
            results["c"] = False
    finally:
        await riot.aclose()

    print("\nSUMMARY:", {k: ("PASS" if v else "FAIL") for k, v in results.items()})
    return 0 if all(results.values()) else 1


def main() -> int:
    if len(sys.argv) < 2 or "#" not in sys.argv[1]:
        print(__doc__)
        return 2
    key = os.environ.get("RIOT_API_KEY")
    if not key:
        print("set RIOT_API_KEY first")
        return 2
    name, _, tag = sys.argv[1].rpartition("#")
    platform = (sys.argv[2] if len(sys.argv) > 2 else "euw1").lower()
    return asyncio.run(run(key, name, tag, platform))


if __name__ == "__main__":
    sys.exit(main())
