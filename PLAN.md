# League of Legends Telegram Tracker — Build Plan

A long-running Python service that watches a configured set of League of Legends
players, notifies a Telegram chat when they finish a *session* of games, and
posts a weekly summary of games played, winrate and total time.

This document is the spec. Build it in the milestone order given — each
milestone is independently verifiable, and milestone 0 exists to de-risk the
rest.

---

## 0. Non-negotiable context (read before writing code)

### 0.1 The API key problem

Riot issues three key types:

| Type | Expiry | How to get it |
|---|---|---|
| Development | **24 hours** | Automatic on signing into the developer portal |
| Personal | Never | Register a product; no verification step, needs a detailed description; ~2 weeks to process |
| Production | Never | Requires a working prototype + approval; not needed here |

A development key **cannot** run this service — it dies every day. Apply for a
**Personal key** at <https://developer.riotgames.com> ("Register Product" →
personal project) on day one, and build against the dev key meanwhile.

Consequences for the design:

- The key must be a single environment variable, swappable with a restart.
- On a `403`, the bot must send a Telegram alert saying the key is likely
  expired (deduped — once per hour maximum, not once per poll).
- **PUUIDs are encrypted per application.** When the personal key is swapped in,
  every stored PUUID silently becomes invalid. See §4.4 for the migration.

### 0.2 Cassiopeia's state

`cassiopeia==5.2.0`, released March 2025. Requires **Python ≥ 3.12**. It has
been migrated to Riot IDs (`cass.get_account(name=..., tagline=...)`) and
match-v5. It is **synchronous**, which matters because the bot is asyncio.

Unverified and load-bearing: **spectator-v5 support**. Riot moved live-game
lookup from spectator-v4 (summoner ID) to spectator-v5 (PUUID). Milestone 0
verifies this against the live API before anything is built on top of it.

### 0.3 Rate limits

Dev and personal keys: **20 requests/second, 100 requests per 2 minutes**. The
polling design below uses roughly `2 × players` calls per poll cycle, so with a
120-second interval and a handful of players there is enormous headroom. Do not
add concurrency for throughput; add it only to avoid blocking the event loop.

---

## 1. Requirements

### Functional

1. Track a configurable list of players by Riot ID (`GameName#TAG`) + platform.
2. Detect completed matches across **all queues** (ranked, normals, ARAM,
   Arena, bots, rotating modes).
3. **Session coalescing:** do not notify per game. Group consecutive games into
   a session and send one notification when the session ends. A session
   continues while the gap between games is under `session_gap_minutes`
   (default 15) — and, critically, while the player is still in a live game.
4. Notification mode is configurable per deployment: `session` (default),
   `per_game`, or `off`.
5. Weekly summary: total games, winrate, total time played, broken down by
   queue group, delivered on a schedule in the local timezone.
6. Runtime management over Telegram: add/remove/list tracked players, on-demand
   stats, live status. Restricted to an admin allowlist.

### Non-functional

- Runs unattended on a VPS under systemd; survives restarts without losing
  session state or re-notifying about already-seen matches.
- Idempotent ingestion — a match is never counted or announced twice.
- No live API calls in the test suite.

---

## 2. Stack

| Concern | Choice | Notes |
|---|---|---|
| Runtime | Python 3.12+ | Cassiopeia floor |
| Telegram | `python-telegram-bot[job-queue] >= 21` | async; JobQueue handles debounce flushes and the weekly cron |
| Riot API | `cassiopeia==5.2.0` behind an adapter | see §3.2 — the adapter is the point |
| Storage | SQLite via `aiosqlite` | single file, no server to run |
| Config | TOML (`tomllib`, stdlib) + env vars for secrets | |
| Scheduling | PTB `JobQueue` | avoids a second scheduler dependency |
| Packaging & execution | **`uv` — exclusively** | see §2.1; no bare `pip`, no manual venv, no system Python |
| Process | systemd unit, `Restart=always` | §9 |

Secrets (`RIOT_API_KEY`, `TELEGRAM_BOT_TOKEN`) come from the environment only —
never from the TOML file, which should be safe to commit.

### 2.1 uv is the only dependency and execution path

**All** dependency management and execution — local, in tests, and in
production on the VPS — goes through `uv`. There is no `requirements.txt`, no
`pip install`, no hand-rolled `python -m venv`, and nothing is ever run against
the system interpreter.

Project definition lives in `pyproject.toml`, with `uv.lock` **committed to the
repo** so the VPS installs byte-identical versions to the dev machine. Given
how stale Cassiopeia is, a reproducible lock is doing real work here — a silent
transitive upgrade is a plausible way for this bot to break.

Pin the interpreter so uv provisions it rather than depending on whatever the
VPS ships:

```toml
# pyproject.toml
requires-python = ">=3.12"
```

```
.python-version      # "3.12" — committed
```

Canonical commands, and the only ones that should appear in the README, in
docs, or in the systemd unit:

| Task | Command |
|---|---|
| Install / sync from lock | `uv sync` |
| Add a dependency | `uv add python-telegram-bot[job-queue]` |
| Add a dev-only dependency | `uv add --dev pytest pytest-asyncio` |
| Run the bot | `uv run python -m lolwatch` |
| Run a CLI subcommand | `uv run python -m lolwatch.cli backfill --days 14` |
| Run the tests | `uv run pytest` |
| Upgrade one dependency deliberately | `uv lock --upgrade-package cassiopeia` |
| Reproduce CI/prod exactly | `uv sync --frozen` |

On the VPS, deployment is `git pull && uv sync --frozen && systemctl restart
lolwatch`. `--frozen` makes the deploy fail loudly if the lockfile and
`pyproject.toml` have drifted apart, instead of quietly resolving something new
in production.

Dev dependencies (`pytest`, `pytest-asyncio`, `ruff`) go in a dev group so
`uv sync --no-dev` on the server installs only what the bot actually needs.

---

## 3. Architecture

### 3.1 Layout

```
lolwatch/
  __init__.py
  __main__.py          # entrypoint: wire config, db, clients, jobs
  config.py            # TOML + env loading, typed dataclasses, validation
  db.py                # schema, migrations, connection management
  models.py            # Player, Match, Session dataclasses
  riot/
    __init__.py
    adapter.py         # RiotClient protocol — the ONLY Riot surface the app sees
    cassiopeia_impl.py # Cassiopeia implementation, thread-offloaded
    errors.py          # RiotAuthError, RiotRateLimited, RiotUnavailable
  poller.py            # periodic match-history scan, ingestion
  sessions.py          # session coalescing + flush decisions (PURE LOGIC)
  notifier.py          # message formatting + Telegram send
  summary.py           # weekly aggregation
  bot/
    __init__.py
    commands.py        # /add /remove /list /status /week /stats
    auth.py            # admin allowlist decorator
  cli.py               # backfill, resolve, repuuid, dry-run-summary
tests/
  fixtures/            # recorded match-v5 / account-v1 JSON
  test_sessions.py     # frozen-clock session logic tests
  test_summary.py
  test_formatting.py
config.example.toml
```

### 3.2 The Riot adapter (important)

Every Riot call goes through a narrow protocol. Nothing else in the codebase
imports `cassiopeia`.

```python
class RiotClient(Protocol):
    async def resolve_account(self, game_name: str, tag_line: str,
                              platform: str) -> AccountInfo: ...
    async def recent_match_ids(self, puuid: str, platform: str,
                               since: datetime | None,
                               count: int = 20) -> list[str]: ...
    async def get_match(self, match_id: str, platform: str) -> MatchData: ...
    async def is_in_game(self, puuid: str, platform: str) -> bool: ...
```

Four methods. That is the entire dependency on Riot.

Rationale: Cassiopeia is 18 months stale and synchronous. If its spectator-v5
support is broken (see milestone 0), or if it breaks on a future Riot change,
swapping in a raw `httpx` implementation means rewriting one file, not the app.

**Cassiopeia is synchronous — never call it directly from the event loop.**
Wrap every call in `asyncio.to_thread(...)` inside `cassiopeia_impl.py`. A
blocking HTTP call on the loop will stall the Telegram bot.

Map HTTP failures to typed errors at this boundary: `403 → RiotAuthError`
(triggers the key-expiry alert), `429 → RiotRateLimited` (back off), `5xx /
timeout → RiotUnavailable` (retry with jitter, do not alert).

### 3.3 Flow

```
poll loop (every poll_interval_seconds, per player)
   └─ recent_match_ids(since=last_seen)
        └─ for each unseen id: get_match() → INSERT into matches
             └─ sessions.assign(match) → open new session OR extend existing
                  └─ (re)schedule flush job at last_match_end + session_gap

flush job fires
   └─ is_in_game(puuid)?
        ├─ yes → postpone by recheck_interval, up to max_postpone
        └─ no  → notifier.send_session_summary() → mark session flushed

weekly job (cron, local tz)
   └─ summary.build(week) → notifier.send_weekly()
```

---

## 4. Data model

SQLite. Times stored as UTC epoch milliseconds (matching Riot's format);
convert to local timezone only at render time.

```sql
CREATE TABLE players (
  puuid          TEXT PRIMARY KEY,
  game_name      TEXT NOT NULL,
  tag_line       TEXT NOT NULL,
  platform       TEXT NOT NULL,        -- euw1, na1, kr ...
  active         INTEGER NOT NULL DEFAULT 1,
  added_by       INTEGER,              -- telegram user id
  added_at       INTEGER NOT NULL,
  last_seen_ms   INTEGER,              -- newest ingested match start
  name_checked_at INTEGER
);

CREATE TABLE matches (
  match_id       TEXT NOT NULL,
  puuid          TEXT NOT NULL,
  queue_id       INTEGER NOT NULL,
  champion       TEXT NOT NULL,
  win            INTEGER NOT NULL,
  remake         INTEGER NOT NULL DEFAULT 0,
  kills          INTEGER, deaths INTEGER, assists INTEGER,
  duration_s     INTEGER NOT NULL,
  start_ms       INTEGER NOT NULL,
  end_ms         INTEGER NOT NULL,
  ingested_at    INTEGER NOT NULL,
  PRIMARY KEY (match_id, puuid)
);
CREATE INDEX idx_matches_puuid_start ON matches(puuid, start_ms);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  puuid          TEXT NOT NULL,
  started_ms     INTEGER NOT NULL,
  last_end_ms    INTEGER NOT NULL,
  status         TEXT NOT NULL,        -- 'open' | 'flushed'
  flush_due_ms   INTEGER,
  postpones      INTEGER NOT NULL DEFAULT 0,
  notified_at    INTEGER
);
CREATE INDEX idx_sessions_open ON sessions(puuid, status);

CREATE TABLE session_matches (
  session_id     INTEGER NOT NULL,
  match_id       TEXT NOT NULL,
  puuid          TEXT NOT NULL,
  PRIMARY KEY (session_id, match_id, puuid)
);

CREATE TABLE subscribers (
  chat_id        INTEGER PRIMARY KEY,
  added_at       INTEGER NOT NULL
);

CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);  -- alert dedupe, schema version
```

### 4.4 PUUID re-resolution

Because PUUIDs are key-scoped, provide `lolwatch.cli repuuid`: for every player,
re-resolve `game_name#tag_line` under the current key and rewrite the `puuid`
column, cascading to `matches` and `sessions`. Store a hash of the active API
key in `kv`; on startup, if the hash changed, log a loud warning telling the
operator to run `repuuid`. This is the difference between a clean key swap and
a silently dead bot.

---

## 5. Session coalescing (the core algorithm)

Implement in `sessions.py` as **pure functions over explicit time inputs** — no
`datetime.now()` inside, no I/O. This is where the subtle bugs live and it must
be unit-testable with a frozen clock.

### Assignment

On ingesting a match `m` for player `p`:

```
open = find_open_session(p)
if open and m.start_ms <= open.last_end_ms + gap_ms:
    attach m to open
    open.last_end_ms = max(open.last_end_ms, m.end_ms)
else:
    if open: flush_immediately(open)      # stale session, e.g. after downtime
    open = create_session(p, m)
open.flush_due_ms = open.last_end_ms + gap_ms
reschedule_flush_job(open)
```

Matches can arrive out of order; always use `max()` and re-derive
`flush_due_ms` from the session's true latest end.

### Flushing

When the flush job for a session fires:

```
if await riot.is_in_game(puuid):
    session.postpones += 1
    if session.postpones > max_postpones:
        flush()                    # spectator may be lying; don't hang forever
    else:
        session.flush_due_ms = now + recheck_interval_ms
        reschedule()
else:
    flush()                        # format + send + status='flushed'
```

`is_in_game` returning an error is **not** a reason to postpone — treat an
unavailable spectator endpoint as "not in game" and flush, so a spectator
outage delays nothing.

### Restart recovery

On startup, load every `status='open'` session and reschedule its flush job. If
`flush_due_ms` is already in the past, run the flush check immediately. This is
what prevents a restart from swallowing a notification.

### Defaults

```toml
session_gap_minutes = 15
flush_recheck_minutes = 5
max_postpones = 18          # ~90 min ceiling
```

### Tests to write (non-optional)

- Two games 5 min apart → one session, one notification.
- Two games 40 min apart → two sessions, two notifications.
- Game ends, flush fires, player is in game → postpones; next game attaches to
  the *same* session rather than starting a new one.
- `max_postpones` exceeded → flushes anyway.
- Out-of-order match arrival → correct `last_end_ms`.
- Process restart with an open session → flush still happens.
- Single game → still exactly one notification.

---

## 6. Match ingestion details

Use match-v5 `by-puuid/{puuid}/ids` with `startTime` set to the player's
`last_seen_ms` (seconds, not ms — Riot takes epoch **seconds** here) and
`count=20`. Fetch full match data only for IDs not already in `matches`.

**Known traps:**

- **`gameDuration` units.** Post-patch-11.20 matches report seconds; older ones
  report milliseconds. Prefer `gameEndTimestamp - gameStartTimestamp` when both
  are present, and sanity-check the result (a >4-hour game is a parsing bug, not
  a long game).
- **Remakes.** `gameEndedInEarlySurrender` or duration under ~5 minutes. Store
  them, flag them, **exclude from winrate**, mention the count separately.
- **Arena / rotating modes** have different participant shapes and team
  semantics — `win` may not mean what it means on Summoner's Rift. Guard the
  parser and fall back to "unknown result" rather than crashing.
- **Match history lag.** Matches appear a minute or two after they end. The
  poller is not late; Riot is.
- **Routing values.** account-v1 and match-v5 use *regional* routing
  (`europe`, `americas`, `asia`); summoner and spectator use *platform*
  routing (`euw1`, `na1`). Map platform → region in one place.
- **Riot IDs change.** Re-resolve display names weekly (`name_checked_at`) so
  notifications don't show stale names. The PUUID remains the identity.

### Queue grouping

Map `queue_id` to a display group in one table: Ranked Solo (420), Ranked Flex
(440), Draft/Normal (400, 430), ARAM (450), Arena (1700, 1710), Bots (830, 840,
850, 870, 880, 890), Clash (700), Other. Unknown IDs render as
`Queue {id}` rather than being dropped — rotating modes appear without warning.

---

## 7. Notifications

Telegram HTML parse mode. Keep under 4096 characters; split the weekly summary
across messages if multiple players push it over.

**Session (multiple games):**

```
🎮 Faker#KR1 — session over
4 games · 2h 14m · 3W 1L

  Azir      Ranked Solo   WIN   8/2/11   31m
  Azir      Ranked Solo   LOSS  4/7/6    28m
  Orianna   Ranked Solo   WIN   11/3/9   42m
  Ziggs     ARAM          WIN   19/8/22  22m
```

**Session (single game):** collapse to one line — no "1 games" header.

**Weekly:**

```
📊 Week of 14–20 Sep

Faker#KR1
  23 games · 11h 42m · 61% WR
    Ranked Solo   16 games · 8h 31m · 63% (10W 6L)
    ARAM           6 games · 2h 48m · 50%
    Arena          1 game  · 23m
  Most played: Azir (7), Orianna (4)
  Longest session: 5 games, Wed evening
  (1 remake excluded from winrate)
```

Formatting lives in its own module with its own tests — string assembly bugs
are cheap to catch and annoying to catch in production.

---

## 8. Telegram commands

All mutating commands are gated on an `admin_user_ids` allowlist in config.
Non-admins get a single polite refusal, never a stack trace or a hint about
what the command would have done.

| Command | Access | Behaviour |
|---|---|---|
| `/start` | admin | Register this chat as a subscriber |
| `/stop` | admin | Unsubscribe this chat |
| `/add GameName#TAG [platform]` | admin | Resolve → validate → insert; confirm with resolved name and level. Platform defaults to config default. |
| `/remove GameName#TAG` | admin | Soft-delete (`active=0`); keep history |
| `/list` | any subscriber | Tracked players, platform, last seen |
| `/status` | any subscriber | Who is in a game right now (live spectator calls) |
| `/week [player]` | any subscriber | Current-week summary on demand |
| `/stats GameName#TAG [days]` | any subscriber | Arbitrary window, default 7 |
| `/settings` | admin | Effective config, key status, next weekly run |

`/week` matters for development: it exercises the entire summary path without
waiting until Sunday.

Reject commands in group chats unless the chat is an explicit subscriber.

---

## 9. Configuration

`config.example.toml` (committed; real `config.toml` gitignored):

```toml
[riot]
default_platform = "euw1"
poll_interval_seconds = 120

[notifications]
mode = "session"              # session | per_game | off
session_gap_minutes = 15
flush_recheck_minutes = 5
max_postpones = 18

[weekly]
enabled = true
day = "sunday"
time = "20:00"
timezone = "Europe/London"

[telegram]
admin_user_ids = [123456789]

[logging]
level = "INFO"
```

Secrets via environment (or a local `.env`, real env wins): `RIOT_API_KEY`, `TELEGRAM_BOT_TOKEN`, optional
`LOLWATCH_DB` (default `./lolwatch.db`).

Validate the whole config at startup and exit non-zero with a clear message on
anything invalid — a systemd service that boots into a broken state and stays
quiet is the worst outcome.

**DST:** schedule the weekly job with a timezone-aware trigger, not a fixed UTC
offset, or the summary drifts by an hour twice a year.

---

## 10. Deployment

systemd unit, `/etc/systemd/system/lolwatch.service`:

```ini
[Unit]
Description=LoL Telegram tracker
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=lolwatch
WorkingDirectory=/opt/lolwatch
EnvironmentFile=/etc/lolwatch/env        # chmod 600, holds the secrets
ExecStartPre=/usr/local/bin/uv sync --frozen --no-dev
ExecStart=/usr/local/bin/uv run --frozen --no-dev python -m lolwatch
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/opt/lolwatch

[Install]
WantedBy=multi-user.target
```

Use an absolute path to the `uv` binary — systemd has no useful `PATH`, and a
per-user uv install under `~/.local/bin` will not be visible to the service
user. Install uv system-wide at `/usr/local/bin/uv` and confirm the `lolwatch`
user can execute it. `ExecStartPre` means a restart always reconciles the
environment with the lockfile, so a deploy that forgot `uv sync` still comes up
correct. `uv` also needs a writable cache: either add
`Environment=UV_CACHE_DIR=/opt/lolwatch/.uv-cache` (covered by the existing
`ReadWritePaths`) or it will fail under `ProtectSystem=strict`.

Include in the README: installing uv, `uv sync`, creating the service user, the
env file, `journalctl -u lolwatch -f`, and how to swap the API key (edit env
file → `systemctl restart` → run
`uv run python -m lolwatch.cli repuuid` if the key *type* changed).

### Operational safety

- On startup, validate both tokens with one cheap call each and refuse to start
  on failure.
- `RiotAuthError` → Telegram alert, deduped to once per hour via `kv`.
- Unhandled exception in the poll loop must **not** kill the loop: log, alert
  once, continue on the next tick.
- Log every notification decision at DEBUG with the session id — when it
  eventually misfires, this is the only thing that will explain why.

---

## 11. Milestones

Build in order. Each ends in something runnable.

**M0 — Spike (do this first, ~30 min).** Throwaway script against the live API
with a dev key — run it with `uv run --with cassiopeia spike.py` so the spike
needs no project setup at all. Verify with Cassiopeia 5.2.0: (a) `get_account` by Riot ID,
(b) match-v5 history + a full match parse, (c) **spectator-v5 live-game lookup
by PUUID**. If (c) doesn't work, implement `is_in_game` with raw `httpx`
against `/lol/spectator/v5/active-games/by-summoner/{puuid}` and note it. Do
not start M1 until the answer is known — the whole session design rests on it.

**M1 — Skeleton.** `uv init` the project with `requires-python = ">=3.12"` and
a committed `.python-version`; add dependencies with `uv add` and commit
`uv.lock`. Then config loading, DB schema + migrations, the `RiotClient`
protocol and its Cassiopeia implementation, typed errors, and
`uv run python -m lolwatch.cli resolve` to look up a Riot ID end to end. No
Telegram yet.

**M2 — Ingestion.** Poll loop, idempotent match storage, `last_seen_ms`
tracking, `uv run python -m lolwatch.cli backfill --days 14` to populate
history. Verify by inspecting the DB. Still no Telegram.

**M3 — Session logic.** `sessions.py` as pure functions, plus the full frozen-
clock test suite from §5. This is the milestone to slow down on.

**M4 — Notifications.** Wire sessions to Telegram, message formatting with
tests, flush job scheduling, restart recovery.

**M5 — Commands.** Admin auth, all commands from §8. `/week` here even though
the weekly job comes next.

**M6 — Weekly summary.** Aggregation, queue breakdown, remake handling,
timezone-aware scheduling, multi-message splitting.

**M7 — Deployment.** systemd unit, README, key-swap runbook, `repuuid`,
health alerts, structured logging.

---

## 12. Definition of done

- Restarting the process mid-session still produces exactly one notification.
- A player playing 5 back-to-back games produces **one** message, sent after
  the last one, not during the fourth.
- Swapping the API key and running `repuuid` leaves the bot fully working.
- `uv run pytest` passes with no network access.
- A clean clone plus `uv sync --frozen` produces a working install on the VPS
  with no other setup steps, and `uv.lock` is committed.
- `journalctl -u lolwatch` shows why every notification fired.
```
