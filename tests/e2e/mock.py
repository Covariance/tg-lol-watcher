"""Mock Riot + Telegram for an end-to-end run of the lolwatch binary.

Telegram:  POST /bot<token>/<method>
Riot:      GET  /<routing value>/<riot path>

Script: an admin sends /start then /add Alice#EUW. Alice's history has one old game
(imported silently). After the bot has polled a few times, a new game appears in her
history that ended a few minutes ago; with a 1-minute session gap the bot should flush
it right away and post a session message. /status and /list exercise the rest.
"""

import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ADMIN = 4242
NOW_MS = int(time.time() * 1000)
OLD = {"id": "EUW1_100", "start": NOW_MS - 3 * 86_400_000, "minutes": 31, "champ": "Azir", "win": True}
NEW = {"id": "EUW1_200", "start": NOW_MS - 40 * 60_000, "minutes": 33, "champ": "Ahri", "win": False}

state = {
    "update_id": 0,
    "script": ["/start", "/add Alice#EUW", "/list"],
    "later": ["/status", "/help"],
    "ids_calls": 0,
    "sent": [],
    "menus": 0,
    "riot_requests": 0,
}
lock = threading.Lock()


def match_json(m):
    end = m["start"] + m["minutes"] * 60_000
    return {
        "metadata": {"matchId": m["id"]},
        "info": {
            "gameCreation": m["start"] - 60_000,
            "gameStartTimestamp": m["start"],
            "gameEndTimestamp": end,
            "gameDuration": m["minutes"] * 60,
            "queueId": 420,
            "participants": [
                {"puuid": "puuid-alice", "championName": m["champ"], "championId": 1, "win": m["win"],
                 "kills": 7, "deaths": 3, "assists": 9, "gameEndedInEarlySurrender": False,
                 "unusedField": {"nested": [1, 2, 3]}},
                {"puuid": "puuid-x", "championName": "Zed", "win": not m["win"], "kills": 1,
                 "deaths": 2, "assists": 3},
            ],
        },
    }


def update(text):
    state["update_id"] += 1
    cmd_len = text.find(" ") if " " in text else len(text)
    return {"update_id": state["update_id"], "message": {
        "message_id": state["update_id"], "date": int(time.time()),
        "from": {"id": ADMIN, "is_bot": False, "first_name": "Op"},
        "chat": {"id": ADMIN, "type": "private"},
        "text": text, "entities": [{"type": "bot_command", "offset": 0, "length": cmd_len}]}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, status, body, headers=None):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json;charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    # -- Telegram ---------------------------------------------------------------
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        method = self.path.rsplit("/", 1)[-1]
        if not self.path.startswith("/botTEST:token/"):
            return self.reply(401, {"ok": False, "error_code": 401, "description": "Unauthorized"})
        with lock:
            if method == "getMe":
                return self.reply(200, {"ok": True, "result": {"id": 1, "is_bot": True, "first_name": "tb", "username": "tb"}})
            if method in ("deleteWebhook", "setMyCommands"):
                state["menus"] += method == "setMyCommands"
                return self.reply(200, {"ok": True, "result": True})
            if method == "sendMessage":
                state["sent"].append((body["chat_id"], body["text"], body.get("parse_mode")))
                print(f"SEND to {body['chat_id']}: {body['text']!r}", flush=True)
                return self.reply(200, {"ok": True, "result": {"message_id": len(state["sent"])}})
            if method == "getUpdates":
                updates = []
                if state["script"]:
                    updates = [update(state["script"].pop(0))]
                elif state["ids_calls"] >= 4 and state["later"]:
                    updates = [update(state["later"].pop(0))]
                if updates:
                    return self.reply(200, {"ok": True, "result": updates})
        time.sleep(min(body.get("timeout", 0), 1))  # a short "long poll"
        return self.reply(200, {"ok": True, "result": []})

    # -- Riot -------------------------------------------------------------------
    def do_GET(self):
        with lock:
            state["riot_requests"] += 1
        if self.headers.get("X-Riot-Token") != "RGAPI-e2e":
            return self.reply(403, {"status": {"status_code": 403}})
        path = self.path.split("?")[0]
        if path == "/europe/riot/account/v1/accounts/by-riot-id/Alice/EUW":
            return self.reply(200, {"puuid": "puuid-alice", "gameName": "Alice", "tagLine": "EUW"})
        if path == "/euw1/lol/summoner/v4/summoners/by-puuid/puuid-alice":
            return self.reply(200, {"summonerLevel": 321, "profileIconId": 1})
        if path == "/europe/riot/account/v1/accounts/by-puuid/puuid-alice":
            return self.reply(200, {"puuid": "puuid-alice", "gameName": "Alice", "tagLine": "EUW"})
        if path == "/europe/lol/match/v5/matches/by-puuid/puuid-alice/ids":
            with lock:
                state["ids_calls"] += 1
                calls = state["ids_calls"]
            if calls == 3:  # one rate limit of the new 'server' kind, as seen in production
                return self.reply(429, {"status": {"status_code": 429}},
                                  {"Retry-After": "1", "X-Rate-Limit-Type": "server"})
            ids = [OLD["id"]] if calls < 4 else [NEW["id"], OLD["id"]]
            return self.reply(200, ids)
        for m in (OLD, NEW):
            if path == f"/europe/lol/match/v5/matches/{m['id']}":
                return self.reply(200, match_json(m))
        if path.startswith("/euw1/lol/spectator/v5/active-games/by-summoner/"):
            return self.reply(404, {"status": {"status_code": 404, "message": "Data not found"}})
        return self.reply(404, {"status": {"status_code": 404}})


def report():
    with lock:
        print(json.dumps({"sent": state["sent"], "menus": state["menus"], "ids_calls": state["ids_calls"],
                          "riot_requests": state["riot_requests"]}), flush=True)


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    duration = float(sys.argv[2])
    time.sleep(duration)
    report()
