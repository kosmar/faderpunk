#!/usr/bin/env bash
# Consistency gate for test/playground WIP assembly.
# See .cursor/skills/faderpunk-firmware-bootsel/ — run after every WIP sync/flash.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
fail=0

ok() { echo "OK  $*"; }
bad() { echo "FAIL $*"; fail=1; }

echo "=== wip-sync-check ==="

# --- per-app docs fragments + assembled cheatsheet freshness ---
echo "=== app docs / cheatsheet assembly ==="
python3 scripts/validate-app-docs.py || exit 1
python3 scripts/assemble-cheatsheet.py --scope wip --check || exit 1

# --- registry vs register_apps! ---
python3 - <<'PY' || exit 1
import re, sys
from pathlib import Path
ids_md = Path("docs/wip-app-ids.md").read_text()
mod = Path("faderpunk/src/apps/mod.rs").read_text()
flash = Path("flash-now.sh").read_text()
cheat = Path("docs/wip-apps-ux-cheatsheet.html").read_text()
rule = Path(".cursor/rules/playground-flash.mdc").read_text()

claimed = []
for m in re.finditer(r"\|\s*(\d+)\s*\|\s*([^|]+)\|", ids_md):
    i = int(m.group(1))
    name = m.group(2).strip()
    if i == 28:
        continue
    if i < 29:
        continue
    claimed.append((i, name))

regs = {int(a): b for a, b in re.findall(r"(\d+)\s*=>\s*(\w+)", mod)}
req = re.findall(r'"([^"]+)"', flash.split("REQUIRED", 1)[1].split(")", 1)[0])

fail = 0
print(f"claimed WIP IDs: {len(claimed)}")
for i, name in claimed:
    if i not in regs:
        print(f"FAIL ID {i} ({name}) missing from register_apps!")
        fail = 1
    else:
        print(f"OK  ID {i} => {regs[i]} ({name})")
    if f"ID {i}" not in cheat:
        print(f"FAIL ID {i} missing from cheatsheet HTML")
        fail = 1
    else:
        print(f"OK  cheatsheet has ID {i}")

# CONFIG names in flash-now must cover claimed display names
# Use App column first word / known names from flash REQUIRED
for name in req:
    if name not in cheat and name.replace(" ", "") not in cheat.replace(" ", ""):
        # soft: name should appear as <h2>
        if f">{name}" not in cheat and f">{name} <" not in cheat:
            print(f"WARN flash REQUIRED '{name}' not obvious in cheatsheet")
    print(f"OK  flash REQUIRED '{name}'")

if "28 =>" in mod or re.search(r"\b28\s*=>", mod):
    print("FAIL ID 28 registered in mod.rs")
    fail = 1
else:
    print("OK  ID 28 not registered")

# Rule list should mention current high IDs
if (
    "Control Issues" not in rule
    or "Venn" not in rule
    or "Bassment" not in rule
    or "Contura" not in rule
):
    print("FAIL playground-flash.mdc missing Control Issues / Venn / Bassment / Contura")
    fail = 1
else:
    print("OK  playground-flash.mdc lists CI + Venn + Bassment + Contura")

# Genre axis (shared Grooves/Vamp)
palette = Path("faderpunk/src/apps/genre_palette.rs").read_text()
if 'NUM_GENRES: usize = 9' not in palette or '"Jungle"' not in palette:
    print("FAIL genre_palette missing 9 genres / Jungle")
    fail = 1
else:
    print("OK  genre_palette has Jungle (9)")
if "Jungle" not in cheat:
    print("FAIL cheatsheet missing Jungle")
    fail = 1
else:
    print("OK  cheatsheet mentions Jungle")

sys.exit(fail)
PY

# --- known-fix markers ---
echo "=== known-fix markers ==="
rg -q 'PERF_CABLE' faderpunk/src/tasks/midi.rs && ok "PERF_CABLE" || bad "PERF_CABLE"
rg -q 'is_config_sysex_packet' faderpunk/src/tasks/midi.rs && ok "is_config_sysex_packet" || bad "is_config_sysex_packet"
rg -q 'PERF_CABLE' faderpunk/src/tasks/configure.rs && ok "configure PERF_CABLE TX" || bad "configure PERF_CABLE TX"
rg -q 'Dedicated high-priority queue for MIDI realtime' faderpunk/src/tasks/midi.rs \
  && ok "MIDI realtime queue" || bad "MIDI realtime queue"
rg -q 'pending_kick' faderpunk/src/apps/grooves.rs && ok "Grooves pending_kick" || bad "Grooves pending_kick"
rg -q 'Clock watch → voice engine' faderpunk/src/apps/vamp.rs && ok "Vamp clock isolate" || bad "Vamp clock isolate"
rg -q 'Clock watch → voice engine' faderpunk/src/apps/arp_de_levy.rs && ok "Arp clock isolate" || bad "Arp clock isolate"
rg -q 'clock_ticker' faderpunk/src/apps/harmonica.rs && ok "Harmonica clock_ticker" || bad "Harmonica clock_ticker"
rg -q 'clock_ticker' faderpunk/src/apps/control_issues.rs && ok "CI clock_ticker" || bad "CI clock_ticker"
rg -q 'use_clock' faderpunk/src/apps/control_issues.rs && bad "CI must not use_clock" || ok "CI no use_clock"

# --- modules for grooves tip ---
[[ -f faderpunk/src/apps/groove.rs ]] && ok "groove.rs" || bad "groove.rs missing"
[[ -f faderpunk/src/apps/led_fx.rs ]] && ok "led_fx.rs" || bad "led_fx.rs missing"
rg -q '^mod groove;' faderpunk/src/apps/mod.rs && ok "mod groove" || bad "mod groove"
rg -q '^mod led_fx;' faderpunk/src/apps/mod.rs && ok "mod led_fx" || bad "mod led_fx"

if [[ "$fail" -ne 0 ]]; then
  echo "=== wip-sync-check FAILED ==="
  exit 1
fi
echo "=== wip-sync-check PASSED ==="
