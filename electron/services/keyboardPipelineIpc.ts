// Single source of truth for the IPC contract of the Win32 stealth keyboard
// pipeline (issue #225 Phase 2/3). main.ts dispatches, ipcHandlers.ts
// registers handlers, preload.ts exposes the renderer API — all three sides
// share these channel names so a typo in any one place is impossible.
//
// The Win32-only hotkey accelerator that toggles type mode lives here too,
// because changing it requires touching the same call sites.

export const KEYBOARD_PIPELINE_CHANNELS = {
  // Renderer → main: install / uninstall the LL hook (type-mode session).
  ENTER_TYPE_MODE: 'overlay:enter-type-mode',
  EXIT_TYPE_MODE: 'overlay:exit-type-mode',
  GET_TYPE_MODE_ACTIVE: 'overlay:get-type-mode-active',

  // Main → renderer: every swallowed keystroke from the LL hook.
  TYPE_MODE_KEY: 'overlay:type-mode-key',

  // Main → renderer: type-mode session lifecycle (entered, idle-exited, ...).
  TYPE_MODE_STATE: 'overlay:type-mode-state',

  // Main → renderer: passive raw-input observer signal — user is typing
  // into another app, surface the "press the hotkey" nudge in the overlay.
  RAW_INPUT_TYPING: 'overlay:raw-input-typing',
} as const;

// Toggle type mode on/off. Default chord chosen for low collision rate
// with browser, IDE, and OS chord menus. Not yet user-customisable; once
// KeybindManager grows a "stealth-input" action group, this constant is
// the one to retire.
export const TYPE_MODE_HOTKEY_ACCELERATOR = 'Control+Shift+Space';
