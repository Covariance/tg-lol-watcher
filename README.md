# tg-lol-watcher

Watches a set of League of Legends players and posts to Telegram when they finish a
**session** of games (not one message per game), plus a weekly summary of games,
winrate and time played.

This branch is the Rust port: one self-contained ~5 MB binary, two threads, ~8 MB resident,
no runtime to install. It reads and writes the same SQLite database as the Python
version, so switching over keeps every player, match and open session.

## Commands

| Task | Command |
|---|---|
| Build | `cargo build --release` (binary: `target/release/lolwatch`) |
| Run the bot | `lolwatch` |
| Look up a Riot ID | `lolwatch resolve "Name#TAG" [--platform kr]` |
| Backfill history (silent) | `lolwatch backfill --days 14` |
| Re-resolve PUUIDs after a key change | `lolwatch repuuid` |
| Print the weekly summary, send nothing | `lolwatch dry-run-summary [--days N]` |
| Check the Riot client against the live API | `lolwatch smoke "Name#TAG" [platform]` |
| Run the tests (no network) | `cargo test` |
| End-to-end against a mock Riot + Telegram | `tests/e2e/run.sh` (needs `python3`) |
| Lint / format | `cargo clippy --all-targets` / `cargo fmt` |

## Setup guide

Local first run, about 15 minutes. Deployment to a server is a separate section below.

### 1. Build

Install Rust with [rustup](https://rustup.rs) (1.85 or newer), then in the repo:

```
cargo build --release
```

The binary is `target/release/lolwatch`. SQLite is compiled in; the only thing the
binary needs from the system is the time zone database (`/usr/share/zoneinfo`, present
on every normal Linux and macOS install; it is built in on Windows).

### 2. Get a Riot API key

1. Sign in at <https://developer.riotgames.com>.
2. Copy the **Development API Key** from the dashboard (`RGAPI-...`). It works at once but **expires every 24 hours**, so use it for setup and testing only.
3. For unattended use, click **Register Product** and apply for a **Personal** key (about 2 weeks). When it arrives, follow *Swapping the API key* below.

### 3. Create the Telegram bot

1. In Telegram, message [@BotFather](https://t.me/BotFather), send `/newbot`, and follow the prompts. Copy the token (`123456:ABC...`).
2. Message [@userinfobot](https://t.me/userinfobot) to get your numeric user id.
3. Optional, for group notifications: add the bot to the group. You subscribe the group in step 7.

### 4. Write the config

```
cp config.example.toml config.toml        # Windows: copy config.example.toml config.toml
```

Edit `config.toml`. The only values you must change:

| Key | Set it to |
|---|---|
| `telegram.admin_user_ids` | your numeric user id from step 3 |
| `riot.default_platform` | your players' server: `euw1`, `na1`, `kr`, `eun1`, ... |
| `weekly.timezone` | your IANA timezone, e.g. `Europe/London` |

`config.toml` is gitignored and holds no secrets. Tracked players are not configured here: add them with `/add` (step 7); the database is the only source.

### 5. Set the secrets

Copy `.env.example` to `.env` and fill in the two secrets. The bot loads `./.env` at start-up; real environment variables take precedence (that is how the systemd unit passes them).

```
cp .env.example .env        # Windows: copy .env.example .env
```

Or export them instead:

```
export RIOT_API_KEY=RGAPI-xxxxxxxx
export TELEGRAM_BOT_TOKEN=123456:ABC-xxxx
```

Optional: `LOLWATCH_DB` (default `./lolwatch.db`) and `LOLWATCH_CONFIG` (default `./config.toml`).

### 6. Check the Riot side

```
lolwatch resolve "Name#TAG" --platform euw1
lolwatch smoke "Name#TAG" euw1
```

- `resolve` prints the PUUID and level. If it fails with 403, the key is wrong or expired.
- `smoke` runs the bot's own Riot client (no retries) through (a) account lookup, (b) match
  history plus a full match parse, (c) live-game lookup, and should end with every check `PASS`.

### 7. Start the bot

```
lolwatch
```

Look for `telegram: connected as @yourbot` and `started: mode=session ...` in the log. Then in Telegram:

1. Send `/start` to the bot from your admin account. In a group, send it there instead; that subscribes the group.
2. Send `/add Name#TAG` (add a platform after it if it differs from the default, e.g. `/add Name#TAG kr`). The bot resolves the account and imports recent games **without announcing them**.
3. Check `/list`, `/status` and `/week`. `/settings` shows the effective config and the next weekly run.

### 8. See a notification quickly (optional)

Play one game with a tracked account, or use a temporary test setup in `config.toml`:

```toml
[riot]
poll_interval_seconds = 30

[notifications]
mode = "per_game"        # one message per game, no waiting for the session gap

[logging]
level = "DEBUG"          # logs every notification decision with its session id
```

Restart the bot. Riot lists a match a minute or two after it ends, so allow for that. Set `mode` back to `session` afterwards.

### Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Exit code 2 with a list of config problems | Fix each listed line in `config.toml` or the environment. All problems are shown at once. |
| `refusing to start: Riot API key rejected (403)` | The dev key expired (24 h). Copy a fresh one, update `RIOT_API_KEY`, restart. |
| `refusing to start: Telegram token validation failed` | Wrong `TELEGRAM_BOT_TOKEN`. Re-copy it from @BotFather. |
| Bot says nothing at all | You are not on the allow list. Ask an admin to `/allow <your id>` (the bot logs the ids it ignores: `ignored a message from user_id=...`). |
| Bot replies "Sorry, I can't do that here." | The command needs an admin, or you used a group that hasn't been `/start`ed. |
| Games never show up | Check `/list` shows the player and `/status` works. Matches appear 1 to 2 minutes after they end. Turn on `DEBUG` logging. |
| `The Riot API key has changed` warning | PUUIDs are per key. Run `lolwatch repuuid`, then restart. |
| Weekly summary sent at the wrong hour | `weekly.timezone` must be an IANA name such as `Europe/London`, not `GMT+1`. |
| `unknown timezone` for a valid name on Linux | The system has no tz database (minimal containers). Install it: `apt install tzdata`. |

## Telegram commands

| Command | Who | What |
|---|---|---|
| `/help` | subscriber | list the commands you can use |
| `/start`, `/stop` | admin | subscribe / unsubscribe this chat |
| `/add GameName#TAG [platform]` | admin | resolve, track, silently import recent history |
| `/remove GameName#TAG` | admin | stop tracking (history kept) |
| `/allow [user_id]`, `/disallow [user_id]` | admin | add / remove a user on the allow list (or reply to their message) |
| `/allowed` | admin | admins and allowed users |
| `/sendweekly` | admin | send the weekly summary (the last 7 days, ending now) to all subscribers |
| `/settings` | admin | effective config, key status, next weekly run |
| `/list` | subscriber | tracked players and when they last played |
| `/status` | subscriber | who is in a game right now |
| `/week [GameName#TAG]` | subscriber | summary of the last 7 days (Mon 00:00 → now on a Sunday) |
| `/stats GameName#TAG [days]` | subscriber | same, for any window (default 7) |

### Who can talk to the bot

Access is default-deny, checked in three places (`src/bot.rs`):

1. **Allow list.** Only admins (`telegram.admin_user_ids` in `config.toml`) and users an admin has
   `/allow`ed (stored in the database) get any reaction. Everyone else is ignored silently, in
   private chats and in groups alike, so strangers learn nothing, not even that the bot is alive.
2. **Role.** Admin commands work for admins only, never for allowed users. Allowed users get
   the read commands (`/help`, `/list`, `/status`, `/week`, `/stats`); if they try more they get
   one polite refusal.
3. **Chat.** Outside a private chat, commands only work in a group an admin has `/start`ed.

Messages from bots, or sent on behalf of a chat or channel (anonymous group admins), never count,
even if their id is listed. Edited messages don't re-run commands.

Getting someone's id: they can message @userinfobot, or you reply to one of their messages in a
chat with the bot and send `/allow`. Being in a subscribed group is not enough on its own: the
sender must be on the list too.

## How sessions work

A game joins the open session if it starts within `session_gap_minutes` of the
session's last game ending. When that gap passes, the bot checks whether the player is
in a live game: if so it postpones (every `flush_recheck_minutes`, at most
`max_postpones` times) and the next game joins the *same* session; if not, it sends
one summary. A failing spectator lookup counts as "not in game". Open sessions are
persisted, so a restart never swallows or duplicates a notification.
`notifications.mode` may be `session`, `per_game` or `off`.

History that predates the bot's first sight of a player (first start, `/add`,
`backfill`) is stored but never announced.

## Deployment (Linux VPS, systemd)

Build on the server, or build elsewhere for the same target and copy the binary.

```bash
# 1. service user + binary + config
sudo useradd --system --home /opt/lolwatch --shell /usr/sbin/nologin lolwatch
sudo install -d -o lolwatch -g lolwatch /opt/lolwatch
sudo install -m 755 target/release/lolwatch /opt/lolwatch/lolwatch
sudo install -m 644 -o lolwatch config.example.toml /opt/lolwatch/config.toml   # then edit it

# 2. secrets
sudo install -d -m 700 /etc/lolwatch
sudo install -m 600 /dev/null /etc/lolwatch/env
sudoedit /etc/lolwatch/env        # RIOT_API_KEY=...  and  TELEGRAM_BOT_TOKEN=...

# 3. service
sudo cp deploy/lolwatch.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now lolwatch
journalctl -u lolwatch -f
```

**Coming from the Python version:** stop it, copy its `lolwatch.db` (and `config.toml`)
into `/opt/lolwatch`, install the binary as above and start the new unit. The schema, the
key fingerprint and every stored row are compatible; open sessions are picked up and
flushed as usual.

**Deploy an update:** rebuild, `sudo install -m 755 target/release/lolwatch /opt/lolwatch/lolwatch`,
`sudo systemctl restart lolwatch`.

Set `[logging] level = "DEBUG"` to log every notification decision with its session id
(`session=12 flush decision: flush=false (in game, postponing (2/18))`).

### Swapping the API key (dev → personal, or on expiry)

1. Edit `/etc/lolwatch/env`, replace `RIOT_API_KEY`.
2. `sudo systemctl restart lolwatch`.
3. **If the key changed type/application** (dev → personal), PUUIDs are encrypted per
   key and every stored one is now invalid. The bot detects the change, logs an error
   and alerts the admins. Then run, as the service user with the same environment:

   ```bash
   sudo systemctl stop lolwatch
   sudo systemd-run --wait --pty --uid=lolwatch \
        -p WorkingDirectory=/opt/lolwatch -p EnvironmentFile=/etc/lolwatch/env \
        /opt/lolwatch/lolwatch repuuid
   sudo systemctl start lolwatch
   ```
   (`systemd-run` reads the root-only env file for you; on a dev box just run
   `lolwatch repuuid` with the variables exported.) `repuuid` re-resolves every
   `Name#TAG`, rewrites the PUUID across all tables and records the new key fingerprint
   (which silences the warning).

A `403` from Riot sends an admin alert ("key probably expired"), at most once an hour.

## Development

```
cargo test                    # ~185 tests, no network
cargo clippy --all-targets
tests/e2e/run.sh              # release binary vs. a mock Riot + Telegram
```

Layout (`src/`):

- `riot/` — the `Riot` trait, typed errors and routing (`mod.rs`); the HTTP client with
  the client-side rate limiter (`client.rs`); retries (`retry.rs`); match-v5 parsing,
  where the duration/remake/Arena traps live (`parsing.rs`).
- `sessions.rs`, `summary.rs`, `formatting.rs` — pure logic, no I/O.
- `flusher.rs` (sessions I/O), `poller.rs` (ingestion), `notifier.rs`, `reports.rs`.
- `bot.rs` — commands and access control; `telegram.rs` — the Bot API slice we use.
- `scheduler.rs` — one heap of timed jobs on wall-clock time; `app.rs` — wiring, start-up
  checks, the two threads.
- `http.rs` — the only place that does HTTP (`ureq` + rustls), behind a trait for tests.

Design choices for a small footprint: no async runtime (a scheduler thread and a
Telegram long-poll thread do all the work), one SQLite connection behind a mutex with
cached prepared statements, match JSON deserialised straight into the few fields used,
and fat LTO with `panic = "abort"` in release builds.

`LOLWATCH_RIOT_BASE_URL` and `LOLWATCH_TELEGRAM_BASE_URL` point the clients somewhere
else (the e2e mock uses them; the latter also works with a local Bot API server).

Known limits: test fixtures under `tests/fixtures/` are hand-built to the match-v5
shape, and the client has only been run against the mock — run `lolwatch smoke` once
with a real key.
