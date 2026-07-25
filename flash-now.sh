#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
UF2=target/thumbv8m.main-none-eabihf/release/faderpunk.uf2
ELF=target/thumbv8m.main-none-eabihf/release/faderpunk
[[ -f "$UF2" ]] || { echo "missing UF2"; exit 1; }
[[ -f "$ELF" && "$ELF" -nt "$UF2" ]] && { echo "ELF newer than UF2"; exit 1; }
echo "=== preflight ==="
strings "$UF2" | rg 'Chord Vamp|MIDI Vamp|Heat Pump|Faderpunk' || true
strings "$UF2" | rg -q 'Chord Vamp' || { echo "ABORT: no Chord Vamp"; exit 1; }
strings "$UF2" | rg -q 'MIDI Vamp' && { echo "ABORT: MIDI Vamp stale"; exit 1; }
strings "$UF2" | rg -q 'Heat Pump' || { echo "ABORT: no Heat Pump"; exit 1; }
[[ -d /Volumes/RP2350 ]] || { echo "ABORT: not BOOTSEL"; exit 1; }
picotool load -x "$UF2"
sleep 2
ls /Volumes/RP2350 2>/dev/null && echo still-bootsel || echo bootsel-gone
ioreg -p IOUSB -w0 2>/dev/null | rg -i 'Faderpunk|RP2350' | head -5 || true
echo Done
