/**
 * Browser entry: exposes window.FpMidi for the vanilla index.html script.
 */
import { pullSetupFromDevice, pushSetupToDevice, pushAppParamsToDevice, pushLiveStructureToDevice, pushGlobalConfigToDevice } from "./setup-io.js?v=1788007168087";
import { faderpunkPortsListed, isUsbWedgeError, USB_WEDGE_ERROR } from "./device.js?v=1788007168087";

window.FpMidi = {
  ready: true,
  faderpunkPortsListed,
  isUsbWedgeError,
  USB_WEDGE_ERROR,
  pullSetupFromDevice,
  pushSetupToDevice,
  pushAppParamsToDevice,
  pushLiveStructureToDevice,
  pushGlobalConfigToDevice,
};

window.dispatchEvent(new Event("fp-midi-ready"));
