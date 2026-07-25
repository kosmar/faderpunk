import type { Param } from "@atov/fp-config";

/** Echolot I/O enum indices (must match firmware CONFIG order). */
export const ECHOLOT_IO_MIDI_MIDI = 0;
export const ECHOLOT_IO_MIDI_CV = 1;
export const ECHOLOT_IO_CV_MIDI = 2;

function paramName(param: Param): string {
  if (param.tag === "MidiIn") return "MIDI In";
  if (param.tag === "MidiOut") return "MIDI Out";
  if ("value" in param && param.value && typeof param.value === "object") {
    const v = param.value as { name?: string };
    if (typeof v.name === "string") return v.name;
  }
  return param.tag;
}

/**
 * Which Echolot params matter for the selected I/O mode.
 * Routing / MIDI Out Pong apply to MIDI→MIDI and CV→MIDI (two MIDI outs);
 * MIDI→CV is single-jack and has no Ping-Pong path.
 */
export function isEcholotParamVisible(param: Param, ioMode: number): boolean {
  const name = paramName(param);
  const hasMidiOutPong =
    ioMode === ECHOLOT_IO_MIDI_MIDI || ioMode === ECHOLOT_IO_CV_MIDI;
  const hasMidiIn = ioMode !== ECHOLOT_IO_CV_MIDI;

  if (name === "Routing" || name === "MIDI Out Pong") {
    return hasMidiOutPong;
  }
  if (param.tag === "MidiIn" || name === "MIDI In CH") {
    return hasMidiIn;
  }
  if (name === "Range") {
    // Jack range only when CV is involved.
    return ioMode !== ECHOLOT_IO_MIDI_MIDI;
  }
  return true;
}
