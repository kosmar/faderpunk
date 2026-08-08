#!/usr/bin/env python3
"""Dry-run future ManualTab generator from docs/apps fragments.

Reads docs/apps/*/manual.json + manual.md and prints ManualAppData-shaped
summaries. Does **not** write under configurator/ (no-configurator-commits).

Usage:
  python3 scripts/gen-manual.py
  python3 scripts/gen-manual.py --id 38
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APPS_DIR = ROOT / "docs" / "apps"


def load_docs(only_id: int | None) -> list[dict]:
    out = []
    for path in sorted(APPS_DIR.glob("*/manual.json")):
        data = json.loads(path.read_text())
        if only_id is not None and int(data["id"]) != only_id:
            continue
        md = path.with_name("manual.md")
        data["_text"] = md.read_text().strip() if md.exists() else ""
        out.append(data)
    return out


def as_manual_app_data(doc: dict) -> dict:
    """Shape compatible with configurator ManualAppData (plain data only)."""
    return {
        "appId": int(doc["id"]),
        "title": doc["name"],
        "description": doc.get("description", ""),
        "icon": (doc.get("icon") or "fader").lower(),
        "color": doc.get("color", "Violet"),
        "params": doc.get("params") or [],
        "storage": doc.get("storage") or [],
        "text": doc.get("_text", ""),
        "channels": doc.get("channels") or [],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--id", type=int, help="only this app id")
    ap.add_argument("--json", action="store_true", help="print JSON array")
    args = ap.parse_args()
    docs = load_docs(args.id)
    if not docs:
        print("no docs/apps fragments found", file=sys.stderr)
        return 1
    payload = [as_manual_app_data(d) for d in docs]
    if args.json:
        json.dump(payload, sys.stdout, indent=2, ensure_ascii=False)
        sys.stdout.write("\n")
    else:
        for item in payload:
            ch = len(item["channels"])
            print(
                f"ID {item['appId']:>2}  {item['title']:<16}  "
                f"params={len(item['params'])}  channels={ch}  "
                f"text={len(item['text'])} chars"
            )
        print(
            "\nDry-run only — no configurator/ writes. "
            "Stock promotion can import this shape later."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
