"""M0 spike — verify Cassiopeia against the LIVE Riot API before trusting the app.

    RIOT_API_KEY=RGAPI-... uv run python scripts/spike.py "GameName#TAG" [platform]

(Or, with no project setup at all: `uv run --with cassiopeia==5.2.0 spike.py ...`.)

Checks, in order:
  a) account-v1 lookup by Riot ID
  b) match-v5 history + one full match parse
  c) spectator-v5 live-game lookup by PUUID  <- the load-bearing unknown
If (c) fails through Cassiopeia but the raw endpoint answers, the script says so:
implement `is_in_game` with httpx against
  /lol/spectator/v5/active-games/by-summoner/{puuid}
and note it in the README. Throwaway: not imported by the app.
"""

from __future__ import annotations

import os
import sys
import traceback

import cassiopeia as cass
from cassiopeia.data import Platform


def step(label: str):
    print(f"\n=== {label}")


def main() -> int:
    if len(sys.argv) < 2 or "#" not in sys.argv[1]:
        print(__doc__)
        return 2
    key = os.environ.get("RIOT_API_KEY")
    if not key:
        print("set RIOT_API_KEY first")
        return 2
    name, _, tag = sys.argv[1].rpartition("#")
    plat = Platform((sys.argv[2] if len(sys.argv) > 2 else "euw1").upper())

    settings = cass.get_default_config()
    settings["pipeline"] = {"RiotAPI": {"api_key": key}}
    settings["logging"]["print_calls"] = False
    cass.apply_settings(settings)
    results: dict[str, bool] = {}

    step("a) get_account by Riot ID")
    try:
        account = cass.get_account(name=name, tagline=tag, region=plat.region)
        puuid = account.puuid
        print(f"OK  {account.name}#{account.tagline}  puuid={puuid[:12]}…")
        results["a"] = True
    except Exception:
        traceback.print_exc()
        print("FAIL: cannot continue without a PUUID (403 => key expired/invalid)")
        return 1

    step("b) match-v5 history + full match parse")
    try:
        history = cass.get_match_history(continent=plat.continent, puuid=puuid, start=0, count=5)
        ids = [m.id for m in history][:5]
        print(f"ids: {ids}")
        match = cass.get_match(id=ids[0], region=plat.region)
        match.load()
        from cassiopeia.core.match import MatchData

        from lolwatch.riot.cassiopeia_impl import core_to_info
        from lolwatch.riot.parsing import parse_match

        parsed = parse_match(f"{plat.value}_{ids[0]}", core_to_info(match._data[MatchData]))
        me = parsed.participants.get(puuid)
        print(f"OK  queue={parsed.queue_id} duration={parsed.duration_s}s remake={parsed.remake} "
              f"me={me}")
        results["b"] = me is not None
    except Exception:
        traceback.print_exc()
        results["b"] = False

    step("c) spectator-v5 by PUUID (via Cassiopeia)")
    try:
        current = cass.get_current_match(
            summoner=cass.get_summoner(puuid=puuid, region=plat.region), region=plat.region)
        print(f"OK  in_game={bool(current.exists)}  (False is the normal answer when idle)")
        results["c"] = True
    except Exception:
        traceback.print_exc()
        results["c"] = False
        step("c') raw spectator-v5 endpoint, to see whether the API or Cassiopeia is at fault")
        try:
            import httpx

            r = httpx.get(
                f"https://{plat.value.lower()}.api.riotgames.com"
                f"/lol/spectator/v5/active-games/by-summoner/{puuid}",
                headers={"X-Riot-Token": key}, timeout=10)
            print(f"raw endpoint -> HTTP {r.status_code} (404 = not in game = endpoint works)")
        except Exception:
            traceback.print_exc()

    print("\nSUMMARY:", {k: ("PASS" if v else "FAIL") for k, v in results.items()})
    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
