#!/usr/bin/env python3
"""Validate per-app docs fragments against registry / CONFIG where possible.

Checks:
  - every docs/apps/*/manual.json has matching manual.md
  - required fields present
  - id/name/slug consistent with directory name
  - for WIP ids in docs/wip-app-ids.md: fragment or legacy HTML exists
  - pilot apps (32, 38, 40) must be fragment-backed (not legacy-only)
  - CONFIG::new("Name") matches manual.json name when the module file exists
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APPS_DIR = ROOT / "docs" / "apps"
LEGACY_DIR = ROOT / "docs" / "cheatsheet" / "legacy"
IDS_MD = ROOT / "docs" / "wip-app-ids.md"
MOD_RS = ROOT / "faderpunk" / "src" / "apps" / "mod.rs"
PILOTS = {32, 38, 40}


def claimed_wip() -> list[tuple[int, str]]:
    text = IDS_MD.read_text()
    out = []
    for m in re.finditer(r"\|\s*(\d+)\s*\|\s*([^|]+)\|", text):
        i = int(m.group(1))
        if i < 29 or i == 28:
            continue
        out.append((i, m.group(2).strip()))
    return out


def register_map() -> dict[int, str]:
    mod = MOD_RS.read_text()
    return {int(a): b for a, b in re.findall(r"(\d+)\s*=>\s*(\w+)", mod)}


def config_name(module: str) -> str | None:
    path = ROOT / "faderpunk" / "src" / "apps" / f"{module}.rs"
    if not path.exists():
        return None
    m = re.search(r'Config::new\(\s*"([^"]+)"', path.read_text())
    return m.group(1) if m else None


def legacy_ids() -> set[int]:
    ids = set()
    if not LEGACY_DIR.is_dir():
        return ids
    for path in LEGACY_DIR.glob("*.html"):
        m = re.search(r"(\d+)-", path.name)
        if m:
            ids.add(int(m.group(1)))
    return ids


def main() -> int:
    fail = 0

    def ok(msg: str) -> None:
        print(f"OK  {msg}")

    def bad(msg: str) -> None:
        nonlocal fail
        print(f"FAIL {msg}")
        fail = 1

    apps: dict[int, dict] = {}
    for path in sorted(APPS_DIR.glob("*/manual.json")):
        folder = path.parent.name
        data = json.loads(path.read_text())
        app_id = int(data["id"])
        apps[app_id] = data
        md = path.with_name("manual.md")
        if not md.exists() or not md.read_text().strip():
            bad(f"{folder}: missing manual.md prose")
        else:
            ok(f"{folder}: manual.md")

        for key in ("name", "slug", "status", "tag", "description", "columns"):
            if key not in data:
                bad(f"{folder}: missing field {key}")

        if not folder.startswith(f"{app_id}-"):
            bad(f"{folder}: directory should start with '{app_id}-'")
        expect_dir_slug = folder.split("-", 1)[1].replace("-", "_")
        if data.get("slug") != expect_dir_slug:
            bad(f"{folder}: slug {data.get('slug')!r} != dir slug {expect_dir_slug!r}")
        else:
            ok(f"{folder}: slug matches directory")

        cols = data.get("columns") or []
        if len(cols) != 3:
            bad(f"{folder}: columns must be length 3 (got {len(cols)})")
        else:
            ok(f"{folder}: 3 columns")

        if not data.get("_skip_channels") and not data.get("channels"):
            # channels optional for cheatsheet, recommended for future manuals
            print(f"WARN {folder}: no channels[] (ok for cheatsheet-only)")

        if app_id in PILOTS and data.get("status") != "wip":
            bad(f"{folder}: pilot should be status=wip for now")

    legacy = legacy_ids()
    regs = register_map()

    for app_id in PILOTS:
        if app_id not in apps:
            bad(f"pilot ID {app_id} missing from docs/apps/")
        elif app_id in legacy:
            bad(f"pilot ID {app_id} must not remain in cheatsheet/legacy/")
        else:
            ok(f"pilot ID {app_id} fragment-backed")

    for app_id, name in claimed_wip():
        if app_id in apps:
            ok(f"WIP ID {app_id} ({name}) has docs/apps fragment")
            doc_name = apps[app_id]["name"]
            if doc_name != name and name not in doc_name and doc_name not in name:
                # wip-app-ids uses human names; allow minor mismatch only if CONFIG matches
                print(f"WARN ID {app_id}: wip-app-ids {name!r} vs fragment {doc_name!r}")
            mod = regs.get(app_id)
            if mod:
                cfg = config_name(mod)
                if cfg and cfg != doc_name:
                    bad(f"ID {app_id}: CONFIG name {cfg!r} != fragment {doc_name!r}")
                elif cfg:
                    ok(f"ID {app_id}: CONFIG name matches ({cfg})")
        elif app_id in legacy:
            ok(f"WIP ID {app_id} ({name}) has legacy HTML")
        else:
            bad(f"WIP ID {app_id} ({name}) missing fragment and legacy")

    # Overlap: same id in both apps/ and legacy is wrong (apps should win only after legacy removed)
    overlap = set(apps) & legacy
    if overlap:
        bad(f"IDs in both apps/ and legacy (remove legacy): {sorted(overlap)}")
    else:
        ok("no apps/ vs legacy id overlap")

    return fail


if __name__ == "__main__":
    raise SystemExit(main())
