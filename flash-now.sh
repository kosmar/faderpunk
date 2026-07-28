#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
UF2=target/thumbv8m.main-none-eabihf/release/faderpunk.uf2
ELF=target/thumbv8m.main-none-eabihf/release/faderpunk
[[ -f "$UF2" ]] || { echo "missing UF2"; exit 1; }
[[ -f "$ELF" && "$ELF" -nt "$UF2" ]] && { echo "ELF newer than UF2"; exit 1; }
echo "=== preflight ==="
# Grep the ELF, not the UF2: UF2 256-byte payload blocks split strings at
# arbitrary offsets. Capture strings once — piping into `rg -q` under
# pipefail fails via SIGPIPE when rg exits on the first match.
ELF_STRINGS=$(strings "$ELF")
rg -q 'Chord Vamp' <<<"$ELF_STRINGS" || { echo "ABORT: no Chord Vamp"; exit 1; }
rg -q 'MIDI Vamp' <<<"$ELF_STRINGS" && { echo "ABORT: MIDI Vamp stale"; exit 1; }
rg -q 'sidechain ducking' <<<"$ELF_STRINGS" || { echo "ABORT: no Heat Pump desc"; exit 1; }
[[ -d /Volumes/RP2350 ]] || { echo "ABORT: not BOOTSEL"; exit 1; }
picotool load -x "$UF2"
sleep 2
ls /Volumes/RP2350 2>/dev/null && echo still-bootsel || echo bootsel-gone
ioreg -p IOUSB -w0 2>/dev/null | rg -i 'Faderpunk|RP2350' | head -5 || true
echo Done
