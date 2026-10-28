# tg-lol-watcher

Watches a set of League of Legends players and posts to Telegram when they finish a
**session** of games (not one message per game), plus a weekly summary of games,
winrate and time played.

Everything — install, run, test, deploy — goes through [`uv`](https://docs.astral.sh/uv/).
There is no `requirements.txt`, no `pip`, and nothing runs on the system interpreter.

## Commands

| Task | Command |
|---|---|
| Install / sync from lock | `uv sync` |
| Run the bot | `uv run python -m lolwatch` |
| Look up a Riot ID | `uv run python -m lolwatch.cli resolve "Name#TAG" [--platform kr]` |
| Backfill history (silent) | `uv run python -m lolwatch.cli backfill --days 14` |
| Re-resolve PUUIDs after a key change | `uv run python -m lolwatch.cli repuuid` |
| Print the weekly summary, send nothing | `uv run python -m lolwatch.cli dry-run-summary` |
| Run the tests (no network) | `uv run pytest` |
| Add a dependency | `uv add <pkg>` (dev: `uv add --dev <pkg>`) |
| Upgrade one dependency deliberately | `uv lock --upgrade-package httpx` |
| Reproduce CI/prod exactly | `uv sync --frozen` |

## Setup guide

Local first run, about 15 minutes. Deployment to a server is a separate section below.

### 1. Install uv and the project

uv is the only tool you need; it downloads Python 3.12 itself.

```
# Windows (PowerShell)
powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"

# macOS / Linux
curl -LsSf https://astral.sh/uv/install.sh | sh
```

Then, in the repo:

```
uv sync
```

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
# macOS / Linux / Git Bash
export RIOT_API_KEY=RGAPI-xxxxxxxx
export TELEGRAM_BOT_TOKEN=123456:ABC-xxxx

# Windows PowerShell (current window only)
$env:RIOT_API_KEY = "RGAPI-xxxxxxxx"
$env:TELEGRAM_BOT_TOKEN = "123456:ABC-xxxx"
```

Optional: `LOLWATCH_DB` (default `./lolwatch.db`) and `LOLWATCH_CONFIG` (default `./config.toml`).

### 6. Check the Riot side (M0 spike)

```
uv run python -m lolwatch.cli resolve "Name#TAG" --platform euw1
uv run python scripts/spike.py "Name#TAG" euw1
```

- `resolve` prints the PUUID and level. If it fails with 403, the key is wrong or expired.
- `spike.py` should print `SUMMARY: {'a': 'PASS', 'b': 'PASS', 'c': 'PASS'}`. If `c` fails, see *M0 spike* below.

### 7. Start the bot

```
uv run python -m lolwatch
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
| `The Riot API key has changed` warning | PUUIDs are per key. Run `uv run python -m lolwatch.cli repuuid`, then restart. |
| Weekly summary sent at the wrong hour | `weekly.timezone` must be an IANA name such as `Europe/London`, not `GMT+1`. |

### Live smoke test (do this before trusting the bot)

The Riot client is tested offline against a mock transport only. Check it against the
live API once with:

```
RIOT_API_KEY=RGAPI-... uv run python scripts/spike.py "GameName#TAG" euw1
```

It runs the app's own client through (a) account lookup, (b) match history + a full
parse, (c) live-game lookup.

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

Access is default-deny, checked in three places (`lolwatch/bot/auth.py`):

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

```bash
# 1. uv, system-wide, so the service user can execute it
curl -LsSf https://astral.sh/uv/install.sh | sudo env UV_INSTALL_DIR=/usr/local/bin sh

# 2. service user + code
sudo useradd --system --home /opt/lolwatch --shell /usr/sbin/nologin lolwatch
sudo git clone <this repo> /opt/lolwatch
sudo cp /opt/lolwatch/config.example.toml /opt/lolwatch/config.toml   # then edit it
sudo chown -R lolwatch: /opt/lolwatch

# 3. secrets
sudo install -d -m 700 /etc/lolwatch
sudo install -m 600 /dev/null /etc/lolwatch/env
sudoedit /etc/lolwatch/env        # RIOT_API_KEY=...  and  TELEGRAM_BOT_TOKEN=...

# 4. service
sudo cp /opt/lolwatch/deploy/lolwatch.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now lolwatch
journalctl -u lolwatch -f
```

The unit runs `uv sync --frozen --no-dev` before every start, so the environment always
matches `uv.lock`; `--frozen` fails loudly if `uv.lock` and `pyproject.toml` drifted.
**Deploy an update:** `cd /opt/lolwatch && sudo -u lolwatch git pull && sudo systemctl restart lolwatch`.

Set `[logging] level = "DEBUG"` to log every notification decision with its session id
(`session=12 flush decision: flush=False (in game, postponing (2/18))`).

### Swapping the API key (dev → personal, or on expiry)

1. Edit `/etc/lolwatch/env`, replace `RIOT_API_KEY`.
2. `sudo systemctl restart lolwatch`.
3. **If the key changed type/application** (dev → personal), PUUIDs are encrypted per
   key and every stored one is now invalid. The bot detects the change, logs a
   CRITICAL line and alerts the admins. Then run, as the service user with the same
   environment:

   ```bash
   sudo systemctl stop lolwatch
   sudo systemd-run --wait --pty --uid=lolwatch \
        -p WorkingDirectory=/opt/lolwatch -p EnvironmentFile=/etc/lolwatch/env \
        -E UV_CACHE_DIR=/opt/lolwatch/.uv-cache \
        -E UV_PYTHON_INSTALL_DIR=/opt/lolwatch/.uv-python \
        /usr/local/bin/uv run --frozen --no-dev python -m lolwatch.cli repuuid
   sudo systemctl start lolwatch
   ```
   (`systemd-run` reads the root-only env file for you; on a dev box just run
   `uv run python -m lolwatch.cli repuuid` with the variables exported.)
   `repuuid` re-resolves every `Name#TAG`, rewrites the PUUID across all tables and
   records the new key fingerprint (which silences the warning).

A `403` from Riot sends an admin alert ("key probably expired"), at most once an hour.

## Development

```
uv sync
uv run pytest        # 130+ tests, no network
uv run ruff check .
```

Layout: `lolwatch/riot/adapter.py` is the only Riot surface the app sees (a thin
`httpx` client in `riot/http_client.py` lives behind it); `sessions.py` is pure logic;
`formatting.py` / `summary.py` are pure; `flusher.py` is the I/O shell around sessions.

Known limits: test fixtures under `tests/fixtures/` are hand-built to the match-v5
shape (no live recording was possible without a key), and the Riot client has only
been exercised offline — see the live smoke test.
