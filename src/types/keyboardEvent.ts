// Canonical shape of a keystroke delivered from the Win32 keyboard pipeline
// (issue #225 Phase 2/3). The Rust napi binding emits this exact shape from
// both the LL hook and the passive raw-input observer; the bridge layer in
// `electron/services/WindowsKeyboardHook.ts`, the preload IPC channel
// 'overlay:type-mode-key', and the renderer hook `useTypeMode` all read
// it. Single source of truth — change the field set here when the napi
// shape changes, and tsc will surface every consumer.
//
// Field semantics:
//   * `vk` / `scancode` — Win32 virtual-key code and scancode.
//   * `down` — true on key down, false on key up.
//   * `alt`/`ctrl`/`shift`/`win` — modifier-down state at the moment of
//     dispatch, tracked from observed transitions in the Rust side (NOT
//     from `GetKeyboardState`, which is per-thread).
//   * `character` — printable result of `ToUnicode` for the LL hook
//     (None for Esc / Backspace / modifier-in-isolation / F-keys / etc.).
//     The raw-input observer always sets this to null — it observes only
//     and the JS side just needs "user typed" + the vk for the nudge UX.
export interface KeyboardHookEvent {
  vk: number;
  scancode: number;
  down: boolean;
  alt: boolean;
  ctrl: boolean;
  shift: boolean;
  win: boolean;
  character: string | null;
}
