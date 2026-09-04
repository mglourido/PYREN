/**
 * `KeyboardEvent.code` <-> Linux evdev keycode.
 *
 * The keys page captures a physical key through the browser's own
 * `keydown`, which reports the W3C `code` values `Keyboard.svelte` already
 * uses as its key ids (`KeyA`, `Digit1`, ...). The daemon's `keymap` module
 * speaks evdev numbers instead - the same vocabulary `hotkey`'s triggers
 * already use - so this is the one place the two meet. The numbers below
 * are `linux/input-event-codes.h`'s, in the same order
 * `pyren_hotkey::devices::key_name` lists the ones it names.
 */
const CODE_TO_KEYCODE: Record<string, number> = {
  Escape: 1,
  Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6,
  Digit6: 7, Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11,
  Minus: 12, Equal: 13, Backspace: 14,
  Tab: 15,
  KeyQ: 16, KeyW: 17, KeyE: 18, KeyR: 19, KeyT: 20,
  KeyY: 21, KeyU: 22, KeyI: 23, KeyO: 24, KeyP: 25,
  BracketLeft: 26, BracketRight: 27,
  Enter: 28,
  ControlLeft: 29,
  KeyA: 30, KeyS: 31, KeyD: 32, KeyF: 33, KeyG: 34,
  KeyH: 35, KeyJ: 36, KeyK: 37, KeyL: 38,
  Semicolon: 39, Quote: 40, Backquote: 41,
  ShiftLeft: 42,
  Backslash: 43,
  KeyZ: 44, KeyX: 45, KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50,
  Comma: 51, Period: 52, Slash: 53,
  ShiftRight: 54,
  NumpadMultiply: 55,
  AltLeft: 56,
  Space: 57,
  CapsLock: 58,
  F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64, F7: 65, F8: 66, F9: 67, F10: 68,
  NumLock: 69,
  ScrollLock: 70,
  Numpad7: 71, Numpad8: 72, Numpad9: 73,
  NumpadSubtract: 74,
  Numpad4: 75, Numpad5: 76, Numpad6: 77,
  NumpadAdd: 78,
  Numpad1: 79, Numpad2: 80, Numpad3: 81,
  Numpad0: 82,
  NumpadDecimal: 83,
  IntlBackslash: 86,
  F11: 87, F12: 88,
  NumpadEnter: 96,
  ControlRight: 97,
  NumpadDivide: 98,
  PrintScreen: 99,
  AltRight: 100,
  Home: 102,
  ArrowUp: 103,
  PageUp: 104,
  ArrowLeft: 105,
  ArrowRight: 106,
  End: 107,
  ArrowDown: 108,
  PageDown: 109,
  Insert: 110,
  Delete: 111,
  Pause: 119,
  MetaLeft: 125,
  MetaRight: 126,
  ContextMenu: 127,
};

const KEYCODE_TO_CODE: Record<number, string> = Object.fromEntries(
  Object.entries(CODE_TO_KEYCODE).map(([code, keycode]) => [keycode, code]),
);

/** `null` for a key with no fixed evdev keycode here (`Fn`: handled by the
 *  embedded controller and never reaches Linux at all). */
export function keycodeFromDomCode(code: string): number | null {
  return CODE_TO_KEYCODE[code] ?? null;
}

export function domCodeFromKeycode(keycode: number): string | null {
  return KEYCODE_TO_CODE[keycode] ?? null;
}
