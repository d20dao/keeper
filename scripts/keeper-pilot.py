"""Read-only, bounded public observations for a remote cost pilot; never reads key/env values."""
import json
from pathlib import Path
import sqlite3
import subprocess
import sys


def main():
    action = sys.argv[1]
    if action == "running":
        result = subprocess.check_output([
            "docker", "ps", "--filter", "label=com.docker.compose.project=d20dao",
            "--filter", "label=com.docker.compose.service=keeper", "--format", "{{.ID}}"
        ], text=True, timeout=10)
        return bool(result.strip())
    if action not in ("ready", "attempts"):
        raise ValueError("Unknown observation")
    mount = subprocess.check_output([
        "docker", "volume", "inspect", "d20dao-state-v1", "--format", "{{.Mountpoint}}"
    ], text=True, timeout=10).strip()
    # keeper-env.ts names the journal keeper-<chain>.sqlite; a hand-written keeper.env may keep keeper.sqlite.
    journals = sorted(Path(mount).glob("keeper*.sqlite"))
    if len(journals) != 1:
        raise ValueError("Expected exactly one keeper journal in the state volume")
    database = journals[0]
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=2) as connection:
        if action == "ready":
            epoch = int(sys.argv[2])
            if not 0 < epoch < 2**63:
                raise ValueError("Invalid epoch")
            return connection.execute("SELECT 1 FROM epoch_work WHERE epoch=? AND state='prepared' AND api IS NOT NULL", (epoch,)).fetchone() is not None
        connection.row_factory = sqlite3.Row
        return [dict(row) for row in connection.execute("SELECT hash,kind,job FROM txs LIMIT 1000")]


if __name__ == "__main__":
    try:
        print(json.dumps(main()))
    except Exception:
        print("Remote pilot observation failed; private data suppressed", file=sys.stderr)
        sys.exit(1)
