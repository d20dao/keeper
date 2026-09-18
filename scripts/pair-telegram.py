"""One-time server pairing. Reads a protected token file; never sends chat messages."""
import argparse
import json
import os
from pathlib import Path
import re
import secrets
import sys
import time
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=900)
    args = parser.parse_args()
    if args.output.exists() or not 30 <= args.seconds <= 1800:
        raise ValueError("Existing pairing or invalid timeout")
    token = args.token_file.read_text().strip()
    if len(token) > 256 or not re.fullmatch(r"[0-9]+:[A-Za-z0-9_-]+", token):
        raise ValueError("Invalid token format")
    if os.name != "nt" and args.token_file.stat().st_mode & 0o077:
        raise ValueError("Token file permissions must be private")
    opener = urllib.request.build_opener(NoRedirect())

    def api(method, parameters):
        request = urllib.request.Request(
            "https://api.telegram.org/bot" + token + "/" + method,
            data=json.dumps(parameters).encode(),
            headers={"Content-Type": "application/json"},
        )
        with opener.open(request, timeout=25) as response:
            raw = response.read(65537)
            if len(raw) > 65536:
                raise ValueError("Response exceeds pairing bound")
            body = json.loads(raw)
            if body.get("ok") is not True:
                raise ValueError("Telegram request failed")
            return body["result"]

    me = api("getMe", {})
    username = me["username"]
    if not re.fullmatch(r"[A-Za-z0-9_]+", username):
        raise ValueError("Invalid bot identity")
    code = "d20dao-" + secrets.token_hex(12)
    started = int(time.time())
    print(json.dumps({"bot": "@" + username, "sendThis": "/pair@" + username + " " + code, "expiresInSeconds": args.seconds}), flush=True)
    offset = 0
    while time.time() - started < args.seconds:
        updates = api("getUpdates", {"offset": offset, "timeout": 20, "limit": 10, "allowed_updates": ["message"]})
        for update in updates:
            offset = max(offset, int(update["update_id"]) + 1)
            message = update.get("message", {})
            if int(message.get("date", 0)) < started:
                continue
            text = message.get("text", "")
            parts = text.split()
            if len(parts) != 2 or parts[0].lower() not in ("/pair", "/pair@" + username.lower()) or parts[1] != code:
                continue
            chat_id = int(message["chat"]["id"])
            if chat_id == 0:
                continue
            result = {"chatId": str(chat_id), "botId": str(me["id"]), "botUsername": username, "pairedAt": int(time.time())}
            args.output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w", encoding="utf8") as file:
                json.dump(result, file, indent=2)
                file.write("\n")
                file.flush()
                os.fsync(file.fileno())
            print(json.dumps({"paired": True, "chatIdSaved": True}), flush=True)
            return
    raise TimeoutError("Pairing window expired")


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print("Pairing stopped; credentials and unrelated messages were not logged.", file=sys.stderr)
        sys.exit(1)
