#!/usr/bin/env python3
"""Build Dozen Ctrl device CC catalog + firmware default tables.

Outputs:
  docs/apps/123-dozen-ctrl/device-cc-catalog.json — full maps (Presetpunk / UI)
  faderpunk/src/apps/dozen_ctrl_defaults.rs — DEFAULT_CCS[device][slot] only
"""
from __future__ import annotations

import csv
import json
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEVICES_DIR = pathlib.Path("/Users/kosmar/Documents/Devices")
MIDI_DIR = pathlib.Path("/Users/kosmar/Projects/faderpunk-tools/presetpunk/midi")
OUT_JSON = ROOT / "docs/apps/123-dozen-ctrl/device-cc-catalog.json"
OUT_RS = ROOT / "faderpunk/src/apps/dozen_ctrl_defaults.rs"

# Order matches Param::Enum variants in dozen_ctrl.rs
DEVICE_SPECS: list[dict] = [
    {"key": "hydrasynth", "label": "Hydrasynth", "json": "device_device_Hydrasynth.json"},
    {"key": "blofeld", "label": "Blofeld", "json": "device_device_Blofeld.json"},
    {"key": "lxr_02", "label": "LXR-02", "json": "device_device_LXR-02.json"},
    {"key": "tempera", "label": "Tempera", "json": "device_device_Tempera.json"},
    {"key": "vector", "label": "Vector", "json": "device_device_Vector.json"},
    {"key": "minitaur", "label": "Minitaur", "json": "device_device_Minitaur.json"},
    {"key": "p_6", "label": "P-6", "json": "device_device_P-6.json"},
    {"key": "drum_3p", "label": "Drum 3P", "json": "device_device_Drum_3P.json"},
    {"key": "streichfett", "label": "Streichfett", "json": "device_device_Streichfett.json"},
    {"key": "polyphrase", "label": "Polyphrase", "json": "device_device_Polyphrase.json"},
    {
        "key": "fm1",
        "label": "M-Vave FM1",
        "stub_ccs": [
            (1, "Mod Wheel"),
            (11, "Expression"),
            (91, "FX Depth"),
            (93, "Chorus"),
            (94, "Delay"),
            (95, "Reverb"),
            (74, "Filter Cutoff"),
            (71, "Filter Res"),
            (73, "Attack"),
            (75, "Decay"),
            (79, "Release"),
            (70, "LFO Rate"),
        ],
    },
    {"key": "digitone", "label": "Digitone", "csv": "Elektron/Digitone.csv"},
    {"key": "digitakt", "label": "Digitakt", "csv": "Elektron/Digitakt.csv"},
    {"key": "syntakt", "label": "Syntakt", "csv": "Elektron/Syntakt.csv"},
    {"key": "deepmind_12", "label": "DeepMind 12", "csv": "Behringer/Deepmind 12.csv"},
    {"key": "minilogue_xd", "label": "Minilogue XD", "csv": "KORG/minilogue xd.csv"},
    {"key": "minifreak", "label": "MiniFreak", "csv": "Arturia/MiniFreak.csv"},
    {"key": "take_5", "label": "Take 5", "csv": "Sequential/Take 5.csv"},
    {"key": "bass_station_ii", "label": "Bass Station II", "csv": "Novation/Bass Station II.csv"},
    {"key": "s_1", "label": "S-1", "csv": "Roland/S-1.csv"},
    {"key": "iridium", "label": "Iridium", "csv": "Waldorf/Iridium.csv"},
    {"key": "subsequent_37", "label": "Subsequent 37", "csv": "Moog/Subsequent 37.csv"},
    {"key": "microkorg", "label": "microKORG", "csv": "KORG/microKORG.csv"},
]

PANEL_HINTS = re.compile(
    r"\b(vol(ume)?|pan|balance|master|level|mute|sustain|hold|portamento|glide|"
    r"program|bank|preset|octave|transpose|tune|fine)\b",
    re.I,
)
HIDDEN_HINTS = re.compile(
    r"\b(macro|mut|mod|lfo|env|fx|send|delay|reverb|filter|cutoff|res|"
    r"depth|morph|warp|mutator|arp|ratio|wet|feedback|tone|drive|keytrack)\b",
    re.I,
)
SKIP_CC = {0, 120, 121, 122, 123, 124, 125, 126, 127}


def load_json_device(path: pathlib.Path) -> list[dict]:
    data = json.loads(path.read_text())
    out: list[dict] = []
    for p in data.get("parameters", []):
        if p.get("type") != "cc":
            continue
        cc = int(p["nr1"])
        if cc in SKIP_CC:
            continue
        out.append(
            {
                "cc": cc,
                "name": p.get("name", f"CC {cc}"),
                "section": "",
            }
        )
    return out


def load_csv_device(path: pathlib.Path) -> list[dict]:
    out: list[dict] = []
    with path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            cc_raw = row.get("cc_msb") or row.get("CC") or row.get("cc")
            if not cc_raw or not str(cc_raw).strip().isdigit():
                continue
            cc = int(cc_raw)
            if cc in SKIP_CC:
                continue
            name = (row.get("parameter_name") or row.get("name") or f"CC {cc}").strip()
            section = (row.get("section") or row.get("group") or "").strip()
            out.append({"cc": cc, "name": name, "section": section})
    # dedupe by cc, keep first
    seen: set[int] = set()
    deduped: list[dict] = []
    for e in out:
        if e["cc"] in seen:
            continue
        seen.add(e["cc"])
        deduped.append(e)
    return deduped


def score_cc(entry: dict) -> int:
    cc = entry["cc"]
    name = entry["name"]
    section = entry.get("section") or ""
    blob = f"{name} {section}"
    score = 0
    if HIDDEN_HINTS.search(blob):
        score += 40
    if section and not PANEL_HINTS.search(section):
        score += 15
    if PANEL_HINTS.search(name):
        score -= 30
    if cc in (7, 8, 10, 64):
        score -= 25
    if "macro" in blob.lower():
        score += 20
    return score


def classify(entry: dict, score: int) -> tuple[bool, str]:
    blob = f"{entry['name']} {entry.get('section', '')}"
    if score >= 35:
        tier = "hidden"
    elif PANEL_HINTS.search(blob):
        tier = "panel"
    else:
        tier = "other"
    recommended = score >= 30 and entry["cc"] not in (7, 64)
    return recommended, tier


def label_for(entry: dict, recommended: bool, tier: str) -> str:
    name = entry["name"]
    section = entry.get("section") or ""
    prefix = ""
    if recommended:
        prefix = "★ "
    elif tier == "panel":
        prefix = "Panel · "
    elif tier == "hidden":
        prefix = "Hidden · "
    if section and section.lower() not in name.lower():
        return f"{prefix}{section} / {name}"
    return f"{prefix}{name}"


def build_device(spec: dict) -> dict:
    ccs: list[dict]
    if "json" in spec:
        path = DEVICES_DIR / spec["json"]
        if not path.exists():
            raise SystemExit(f"missing device json: {path}")
        ccs = load_json_device(path)
    elif "csv" in spec:
        path = MIDI_DIR / spec["csv"]
        if not path.exists():
            raise SystemExit(f"missing csv: {path}")
        ccs = load_csv_device(path)
    else:
        ccs = [
            {"cc": cc, "name": name, "section": "FX"}
            for cc, name in spec["stub_ccs"]
        ]

    scored = [(score_cc(e), e) for e in ccs]
    scored.sort(key=lambda t: (-t[0], t[1]["cc"]))

    catalog_entries: list[dict] = []
    recommended_ccs: list[int] = []
    for score, entry in scored:
        rec, tier = classify(entry, score)
        if rec and len(recommended_ccs) < 12:
            recommended_ccs.append(entry["cc"])
        catalog_entries.append(
            {
                "cc": entry["cc"],
                "label": label_for(entry, rec, tier),
                "recommended": rec,
                "tier": tier,
                "section": entry.get("section") or "",
            }
        )

    # Pad defaults if thin map
    while len(recommended_ccs) < 12:
        for entry in sorted(ccs, key=lambda e: e["cc"]):
            if entry["cc"] not in recommended_ccs:
                recommended_ccs.append(entry["cc"])
            if len(recommended_ccs) >= 12:
                break
        if len(ccs) == 0:
            recommended_ccs.extend([1, 11, 74, 71, 91, 93, 1, 11, 74, 71, 91, 93][: 12 - len(recommended_ccs)])
        if len(recommended_ccs) >= 12:
            break

    # Non-recommended tail for full picker list
    rest = [e for e in catalog_entries if not e["recommended"]]
    rest.sort(key=lambda e: e["cc"])
    ordered = [e for e in catalog_entries if e["recommended"]] + rest

    return {
        "id": spec["key"],
        "name": spec["label"],
        "defaults": recommended_ccs[:12],
        "ccs": ordered,
    }


def write_rust(devices: list[dict]) -> None:
    lines = [
        "// @generated by scripts/gen-dozen-ctrl-device-catalog.py — do not edit",
        "",
        f"pub const DEVICE_COUNT: usize = {len(devices)};",
        "",
        "pub const DEVICE_NAMES: &[&str] = &[",
    ]
    for d in devices:
        lines.append(f'    "{d["name"]}",')
    lines.append("];")
    lines.append("")
    lines.append("/// Default CC numbers for slots M1–M4, A1–A4, T1–T4 per device.")
    lines.append("pub const DEFAULT_CCS: [[u8; 12]; DEVICE_COUNT] = [")
    for d in devices:
        vals = ", ".join(str(c) for c in d["defaults"])
        lines.append(f"    [{vals}],")
    lines.append("];")
    lines.append("")
    OUT_RS.write_text("\n".join(lines) + "\n")


def main() -> None:
    devices = [build_device(s) for s in DEVICE_SPECS]
    OUT_JSON.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "version": 1,
        "appId": 123,
        "app": "Dozen Ctrl",
        "note": "Full CC maps for param pickers; ★ = suggested slot defaults.",
        "devices": [
            {
                "id": d["id"],
                "name": d["name"],
                "defaults": d["defaults"],
                "ccs": d["ccs"],
            }
            for d in devices
        ],
    }
    OUT_JSON.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    write_rust(devices)
    print(f"wrote {OUT_JSON}")
    print(f"wrote {OUT_RS}")


if __name__ == "__main__":
    main()
