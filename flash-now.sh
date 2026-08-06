#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
UF2=target/thumbv8m.main-none-eabihf/release/faderpunk.uf2
ELF=target/thumbv8m.main-none-eabihf/release/faderpunk
[[ -f "$UF2" ]] || { echo "missing UF2"; exit 1; }
[[ -f "$ELF" && "$ELF" -nt "$UF2" ]] && { echo "ELF newer than UF2"; exit 1; }
echo "=== preflight ==="
# Grep the ELF binary (not UF2 / not `strings`): UF2 splits payloads, and
# `strings` drops non-ASCII so names like "Arp de Lévy" vanish.
# Keep in sync with docs/wip-app-ids.md (IDs 29–40; never 28/Sift).
REQUIRED=(
  "Heat Pump"
  "Grooves"
  "Golden Gate"
  "Super LFO"
  "Echolot"
  "Arp de Lévy"
  "Chord Vamp"
  "Hold Sam"
  "Harmonica"
  "Loop de Cay"
  "Control Issues"
  "Venn"
)
for name in "${REQUIRED[@]}"; do
  rg -aF --quiet "$name" "$ELF" || {
    echo "ABORT: missing WIP app string: $name"
    exit 1
  }
  echo "OK  $name"
done
rg -aF --quiet "MIDI Vamp" "$ELF" && {
  echo "ABORT: MIDI Vamp stale"
  exit 1
}
[[ -d /Volumes/RP2350 ]] || { echo "ABORT: not BOOTSEL"; exit 1; }
picotool load -x "$UF2"
sleep 2
ls /Volumes/RP2350 2>/dev/null && echo still-bootsel || echo bootsel-gone
ioreg -p IOUSB -w0 2>/dev/null | rg -i 'Faderpunk|RP2350' | head -5 || true
echo Done
