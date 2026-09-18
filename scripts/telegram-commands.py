"""Register the operator chat's command menu without interrupting keeper polling."""
import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RuntimeError("Telegram redirect refused")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--config-file", type=Path, required=True)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    token = args.token_file.read_text().strip()
    if not re.fullmatch(r"[0-9]+:[A-Za-z0-9_-]+", token):
        raise RuntimeError("Invalid bot credential")
    ids = []
    with args.config_file.open() as config:
        for line in config:
            match = re.fullmatch(r"\s*(?:export\s+)?TELEGRAM_CHAT_ID\s*=\s*['\"]?(-?[0-9]+)['\"]?\s*", line)
            if match:
                ids.append(int(match[1]))
    if len(ids) != 1 or ids[0] == 0:
        raise RuntimeError("Missing or duplicate operator chat")
    chat_id = ids[0]
    opener = urllib.request.build_opener(NoRedirect())

    def api(method, payload):
        request = urllib.request.Request(
            "https://api.telegram.org/bot" + token + "/" + method,
            data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with opener.open(request, timeout=10) as response:
            raw = response.read(65537)
        if len(raw) > 65536:
            raise RuntimeError("Telegram response too large")
        result = json.loads(raw)
        if result.get("ok") is not True:
            raise RuntimeError("Telegram operation rejected")
        return result["result"]

    scope = {"type": "chat", "chat_id": chat_id}
    chat_type = api("getChat", {"chat_id": chat_id})["type"]
    if chat_type not in ("private", "group", "supergroup"):
        raise RuntimeError("Unsupported command chat")
    commands = [
        {"command": "status", "description": "Service status and recent activity"},
        {"command": "keeper", "description": "Wallet address, balance and network"},
    ]
    current = api("getMyCommands", {"scope": scope, "language_code": ""})
    if not args.apply:
        print(json.dumps({"apply": False, "scope": "configured_chat", "chat_type": chat_type,
                          "current_commands": [c["command"] for c in current],
                          "desired_commands": [c["command"] for c in commands]}))
        return
    api("setMyCommands", {"scope": scope, "language_code": "", "commands": commands})
    # Keep any existing per-language override consistent with this dedicated bot.
    for language in ("tr", "en"):
        if api("getMyCommands", {"scope": scope, "language_code": language}):
            api("setMyCommands", {"scope": scope, "language_code": language, "commands": commands})
    if chat_type == "private":
        api("setChatMenuButton", {"chat_id": chat_id, "menu_button": {"type": "commands"}})
        if api("getChatMenuButton", {"chat_id": chat_id}).get("type") != "commands":
            raise RuntimeError("Menu confirmation failed")
    if api("getMyCommands", {"scope": scope, "language_code": ""}) != commands:
        raise RuntimeError("Command confirmation failed")
    print(json.dumps({"registered": True, "scope": "configured_chat",
                      "commands": [c["command"] for c in commands], "chat_type": chat_type}))


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # urllib errors can embed the credential-bearing URL. Never print them.
        print("Telegram menu operation failed; credentials and chat details were not logged.", file=sys.stderr)
        sys.exit(1)
