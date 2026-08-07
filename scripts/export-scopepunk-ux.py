#!/usr/bin/env python3
"""Export docs/apps/*/manual.json → Scopepunk app-ux.json catalog.

Skips Param(s) sections (Scopepunk already shows live AppState). Strips
nothing else — Scopepunk renders a safe <strong>-only subset.

Usage:
  python3 scripts/export-scopepunk-ux.py
  python3 scripts/export-scopepunk-ux.py -o ../faderpunk-tools/scopepunk/public/app-ux.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APPS_DIR = ROOT / "docs" / "apps"
DEFAULT_OUT = (
    ROOT.parent / "faderpunk-tools" / "scopepunk" / "public" / "app-ux.json"
)

PARAM_HEADING = re.compile(r"^params?\b", re.I)

CHANNEL_KEYS = (
    "jackTitle",
    "jackDescription",
    "faderTitle",
    "faderDescription",
    "faderPlusShiftTitle",
    "faderPlusShiftDescription",
    "faderPlusFnTitle",
    "faderPlusFnDescription",
    "fnTitle",
    "fnDescription",
    "fnPlusShiftTitle",
    "fnPlusShiftDescription",
    "ledTop",
    "ledBottom",
)


def skip_section(heading: str) -> bool:
    return bool(PARAM_HEADING.match(heading.strip()))


def export_doc(doc: dict) -> dict:
    sections: list[dict] = []
    for col in doc.get("columns") or []:
        for section in col.get("sections") or []:
            heading = str(section.get("heading") or "").strip()
            if not heading or skip_section(heading):
                continue
            items = [str(i) for i in (section.get("items") or []) if str(i).strip()]
            if items:
                sections.append({"heading": heading, "items": items})

    channels: list[dict] = []
    for ch in doc.get("channels") or []:
        slim = {k: ch[k] for k in CHANNEL_KEYS if ch.get(k)}
        if slim:
            channels.append(slim)

    return {
        "id": int(doc["id"]),
        "name": doc.get("name") or "",
        "blurb": doc.get("description") or "",
        "sections": sections,
        "channels": channels,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "-o",
        "--out",
        type=Path,
        default=DEFAULT_OUT,
        help=f"output path (default: {DEFAULT_OUT})",
    )
    args = ap.parse_args()

    apps: dict[str, dict] = {}
    for path in sorted(APPS_DIR.glob("*/manual.json")):
        doc = json.loads(path.read_text())
        entry = export_doc(doc)
        apps[str(entry["id"])] = entry

    if not apps:
        print("no docs/apps fragments found", file=sys.stderr)
        return 1

    payload = {"version": 1, "apps": apps}
    out: Path = args.out
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {out} ({len(apps)} apps)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
