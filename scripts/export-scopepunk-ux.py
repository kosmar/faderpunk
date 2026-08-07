#!/usr/bin/env python3
"""Export Scopepunk app-ux.json from cheatsheet + ManualTab sources.

Merge priority (higher wins for overlapping fields):
  1. docs/apps/*/manual.json          — structured WIP fragments
  2. docs/cheatsheet/legacy/*.html    — WIP cheatsheet HTML
  3. configurator ManualTab.tsx       — stock (+ some WIP) channel manuals

Skips Param(s)-only sections (Scopepunk shows live AppState). Keeps gesture /
LED / jack sections. From ManualTab-only apps, synthesizes Faders/Buttons
sections from channel role fields when no cheatsheet columns exist.

Usage:
  python3 scripts/export-scopepunk-ux.py
  python3 scripts/export-scopepunk-ux.py --scope all|wip|stock
  python3 scripts/export-scopepunk-ux.py -o ../faderpunk-tools/scopepunk/public/app-ux.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APPS_DIR = ROOT / "docs" / "apps"
LEGACY_DIR = ROOT / "docs" / "cheatsheet" / "legacy"
MANUAL_TAB = ROOT / "configurator" / "src" / "components" / "ManualTab.tsx"
DEFAULT_OUT = (
    ROOT.parent / "faderpunk-tools" / "scopepunk" / "public" / "app-ux.json"
)

# Stock apps on main are IDs 1–27 (see docs/wip-app-ids.md). WIP is 29+.
STOCK_MAX_ID = 27
SIFT_ID = 28

PARAM_HEADING = re.compile(r"^params?\b", re.I)
PARAMISH_HEADING = re.compile(
    r"^(params?\b|leds?\s*/\s*other\s*params|other\s*params)",
    re.I,
)

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
    "ledTopPlusShift",
    "ledBottom",
)


def skip_section(heading: str) -> bool:
    h = heading.strip()
    return bool(PARAM_HEADING.match(h) or PARAMISH_HEADING.match(h))


def export_from_columns(doc: dict) -> list[dict]:
    sections: list[dict] = []
    for col in doc.get("columns") or []:
        for section in col.get("sections") or []:
            heading = str(section.get("heading") or "").strip()
            if not heading or skip_section(heading):
                continue
            items = [str(i) for i in (section.get("items") or []) if str(i).strip()]
            if items:
                sections.append({"heading": heading, "items": items})
    return sections


def slim_channels(raw: list[dict] | None) -> list[dict]:
    channels: list[dict] = []
    for ch in raw or []:
        slim = {k: ch[k] for k in CHANNEL_KEYS if ch.get(k)}
        if slim:
            channels.append(slim)
    return channels


def synth_sections_from_channels(channels: list[dict]) -> list[dict]:
    """Build Faders / Buttons lists from ManualTab channel role fields."""
    fader_items: list[str] = []
    button_items: list[str] = []
    for i, ch in enumerate(channels):
        prefix = f"Ch{i} " if len(channels) > 1 else ""

        def line(label: str, title: str | None, desc: str | None) -> str | None:
            if not title and not desc:
                return None
            body = " — ".join(p for p in (title, desc) if p)
            return f"<strong>{prefix}{label}</strong> — {body}"

        for label, t, d in (
            ("Main", ch.get("faderTitle"), ch.get("faderDescription")),
            ("Alt", ch.get("faderPlusShiftTitle"), ch.get("faderPlusShiftDescription")),
            ("Third", ch.get("faderPlusFnTitle"), ch.get("faderPlusFnDescription")),
        ):
            item = line(label, t, d)
            if item:
                fader_items.append(item)
        for label, t, d in (
            ("Btn", ch.get("fnTitle"), ch.get("fnDescription")),
            ("Shift+Btn", ch.get("fnPlusShiftTitle"), ch.get("fnPlusShiftDescription")),
        ):
            item = line(label, t, d)
            if item:
                button_items.append(item)

    out: list[dict] = []
    if fader_items:
        out.append({"heading": "Faders", "items": fader_items})
    if button_items:
        out.append({"heading": "Buttons", "items": button_items})
    return out


def make_entry(
    app_id: int,
    name: str,
    blurb: str,
    sections: list[dict] | None = None,
    channels: list[dict] | None = None,
) -> dict:
    secs = list(sections or [])
    chans = list(channels or [])
    if not secs and chans:
        secs = synth_sections_from_channels(chans)
    return {
        "id": app_id,
        "name": name,
        "blurb": blurb,
        "sections": secs,
        "channels": chans,
    }


def merge_entry(base: dict | None, overlay: dict) -> dict:
    """Overlay wins non-empty fields; sections/channels prefer richer overlay."""
    if base is None:
        return overlay
    name = overlay.get("name") or base.get("name") or ""
    blurb = overlay.get("blurb") or base.get("blurb") or ""
    sections = overlay.get("sections") or base.get("sections") or []
    channels = overlay.get("channels") or base.get("channels") or []
    if not sections and channels:
        sections = synth_sections_from_channels(channels)
    return make_entry(int(overlay["id"]), name, blurb, sections, channels)


# --- docs/apps fragments -----------------------------------------------------


def load_fragments() -> dict[int, dict]:
    out: dict[int, dict] = {}
    if not APPS_DIR.is_dir():
        return out
    for path in sorted(APPS_DIR.glob("*/manual.json")):
        doc = json.loads(path.read_text())
        app_id = int(doc["id"])
        out[app_id] = make_entry(
            app_id,
            doc.get("name") or "",
            doc.get("description") or "",
            export_from_columns(doc),
            slim_channels(doc.get("channels")),
        )
    return out


# --- legacy cheatsheet HTML --------------------------------------------------


class LegacyAppParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.blurb = ""
        self.name = ""
        self.sections: list[dict] = []
        self._in_desc = False
        self._in_h2 = False
        self._in_h3 = False
        self._h2_parts: list[str] = []
        self._h3_parts: list[str] = []
        self._h3: str | None = None
        self._capture_li = False
        self._li_parts: list[str] = []
        self._in_strong = False
        self._in_manual = False

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        cls = (dict(attrs).get("class") or "").split()
        if tag == "span" and "desc" in cls:
            self._in_desc = True
        elif tag == "h2":
            self._in_h2 = True
            self._h2_parts = []
        elif tag == "h3" and not self._in_manual:
            self._in_h3 = True
            self._h3_parts = []
        elif tag == "li" and self._h3 and not self._in_manual:
            self._capture_li = True
            self._li_parts = []
        elif tag == "strong" and self._capture_li:
            self._li_parts.append("<strong>")
            self._in_strong = True
        elif tag == "div" and "manual" in cls:
            self._in_manual = True

    def handle_endtag(self, tag: str) -> None:
        if tag == "span" and self._in_desc:
            self._in_desc = False
        elif tag == "h2" and self._in_h2:
            self._in_h2 = False
            text = "".join(self._h2_parts)
            self.name = re.sub(
                r"\s*(ready|draft)\s*$", "", text, flags=re.I
            ).strip()
        elif tag == "h3" and self._in_h3:
            self._in_h3 = False
            self._h3 = "".join(self._h3_parts).strip()
            self.sections.append({"heading": self._h3, "items": []})
        elif tag == "strong" and self._in_strong:
            self._li_parts.append("</strong>")
            self._in_strong = False
        elif tag == "li" and self._capture_li:
            self._capture_li = False
            item = "".join(self._li_parts).strip()
            if self.sections and item:
                self.sections[-1]["items"].append(item)
        elif tag == "div" and self._in_manual:
            # End of manual block — leave flag; nested divs rare in fragments.
            self._in_manual = False

    def handle_data(self, data: str) -> None:
        if self._in_desc:
            self.blurb += data
        elif self._in_h2:
            self._h2_parts.append(data)
        elif self._in_h3:
            self._h3_parts.append(data)
        elif self._capture_li:
            self._li_parts.append(data)


def load_legacy() -> dict[int, dict]:
    out: dict[int, dict] = {}
    if not LEGACY_DIR.is_dir():
        return out
    for path in sorted(LEGACY_DIR.glob("*.html")):
        html = path.read_text()
        m = re.search(r"ID\s+(\d+)", html)
        if not m:
            raise SystemExit(f"legacy fragment missing ID: {path}")
        app_id = int(m.group(1))
        parser = LegacyAppParser()
        parser.feed(html)
        sections = [
            s for s in parser.sections if s["items"] and not skip_section(s["heading"])
        ]
        out[app_id] = make_entry(
            app_id,
            parser.name or path.stem,
            parser.blurb.strip(),
            sections,
            [],
        )
    return out


# --- ManualTab.tsx -----------------------------------------------------------


def _read_ts_string(src: str, start: int) -> tuple[str, int] | None:
    """Read a TS/JS string or template literal starting at start. Returns (value, end)."""
    if start >= len(src):
        return None
    q = src[start]
    if q not in "\"'`":
        return None
    i = start + 1
    out: list[str] = []
    while i < len(src):
        c = src[i]
        if c == "\\" and i + 1 < len(src):
            out.append(src[i + 1])
            i += 2
            continue
        if q == "`" and c == "$" and i + 1 < len(src) and src[i + 1] == "{":
            # skip ${...} interpolation as empty
            depth = 1
            i += 2
            while i < len(src) and depth:
                if src[i] == "{":
                    depth += 1
                elif src[i] == "}":
                    depth -= 1
                i += 1
            continue
        if c == q:
            return "".join(out), i + 1
        out.append(c)
        i += 1
    return None


def _extract_object_fields(obj_src: str) -> dict[str, str]:
    """Pull top-level string fields from a TS object literal body."""
    fields: dict[str, str] = {}
    for key in (
        "title",
        "description",
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
        "ledTopPlusShift",
        "ledBottom",
    ):
        m = re.search(rf"\b{key}\s*:\s*", obj_src)
        if not m:
            continue
        got = _read_ts_string(obj_src, m.end())
        if got:
            fields[key] = got[0].strip()
    return fields


def _split_channel_objects(channels_src: str) -> list[str]:
    """Split channels: [ {...}, {...} ] body into object source strings."""
    objs: list[str] = []
    i = 0
    n = len(channels_src)
    while i < n:
        if channels_src[i] != "{":
            i += 1
            continue
        depth = 0
        start = i
        in_str = False
        quote = ""
        while i < n:
            c = channels_src[i]
            if in_str:
                if c == "\\" and i + 1 < n:
                    i += 2
                    continue
                if c == quote:
                    in_str = False
                i += 1
                continue
            if c in "\"'`":
                in_str = True
                quote = c
                i += 1
                continue
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    objs.append(channels_src[start + 1 : i])
                    i += 1
                    break
            i += 1
    return objs


def load_manual_tab() -> dict[int, dict]:
    out: dict[int, dict] = {}
    if not MANUAL_TAB.is_file():
        print(f"WARN: ManualTab not found at {MANUAL_TAB}", file=sys.stderr)
        return out
    src = MANUAL_TAB.read_text()
    # Each app starts at "appId: N"
    for m in re.finditer(r"\bappId:\s*(\d+)\s*,", src):
        app_id = int(m.group(1))
        # Walk back to opening brace of this object
        brace = src.rfind("{", 0, m.start())
        if brace < 0:
            continue
        depth = 0
        i = brace
        in_str = False
        quote = ""
        end = -1
        while i < len(src):
            c = src[i]
            if in_str:
                if c == "\\" and i + 1 < len(src):
                    i += 2
                    continue
                if c == quote:
                    in_str = False
                i += 1
                continue
            if c in "\"'`":
                in_str = True
                quote = c
                i += 1
                continue
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    end = i
                    break
            i += 1
        if end < 0:
            continue
        body = src[brace + 1 : end]
        fields = _extract_object_fields(body)
        channels: list[dict] = []
        ch_m = re.search(r"\bchannels\s*:\s*\[", body)
        if ch_m:
            # find matching ]
            start = ch_m.end()
            depth = 1
            j = start
            in_s = False
            q = ""
            while j < len(body) and depth:
                c = body[j]
                if in_s:
                    if c == "\\" and j + 1 < len(body):
                        j += 2
                        continue
                    if c == q:
                        in_s = False
                    j += 1
                    continue
                if c in "\"'`":
                    in_s = True
                    q = c
                    j += 1
                    continue
                if c == "[":
                    depth += 1
                elif c == "]":
                    depth -= 1
                j += 1
            ch_body = body[start : j - 1]
            for obj in _split_channel_objects(ch_body):
                ch_fields = _extract_object_fields(obj)
                slim = {k: ch_fields[k] for k in CHANNEL_KEYS if ch_fields.get(k)}
                if slim:
                    channels.append(slim)
        out[app_id] = make_entry(
            app_id,
            fields.get("title") or f"App {app_id}",
            fields.get("description") or "",
            [],
            channels,
        )
    return out


# --- assemble ----------------------------------------------------------------


def classify(app_id: int) -> str:
    if app_id == SIFT_ID:
        return "skip"
    if 1 <= app_id <= STOCK_MAX_ID:
        return "stock"
    if app_id >= 29:
        return "wip"
    return "skip"


def build_catalog(scope: str) -> dict[str, dict]:
    # Low → high priority
    merged: dict[int, dict] = {}
    for source in (load_manual_tab(), load_legacy(), load_fragments()):
        for app_id, entry in source.items():
            merged[app_id] = merge_entry(merged.get(app_id), entry)

    apps: dict[str, dict] = {}
    for app_id, entry in sorted(merged.items()):
        kind = classify(app_id)
        if kind == "skip":
            continue
        if scope == "stock" and kind != "stock":
            continue
        if scope == "wip" and kind != "wip":
            continue
        # all → stock + wip
        apps[str(app_id)] = entry
    return apps


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "-o",
        "--out",
        type=Path,
        default=DEFAULT_OUT,
        help=f"output path (default: {DEFAULT_OUT})",
    )
    ap.add_argument(
        "--scope",
        choices=("all", "wip", "stock"),
        default="all",
        help="which apps to include (default: all)",
    )
    args = ap.parse_args()

    apps = build_catalog(args.scope)
    if not apps:
        print("no apps exported", file=sys.stderr)
        return 1

    payload = {"version": 1, "apps": apps}
    out: Path = args.out
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")

    stock_n = sum(1 for i in apps if classify(int(i)) == "stock")
    wip_n = sum(1 for i in apps if classify(int(i)) == "wip")
    print(
        f"wrote {out} ({len(apps)} apps: {stock_n} stock, {wip_n} wip; scope={args.scope})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
