// Stealth keyboard pipeline for Windows.
//
// Two distinct subsystems live here, both Win32-only:
//
//   * `win_hook`  — opt-in WH_KEYBOARD_LL low-level hook that swallows
//                   keystrokes and forwards them to JS via a napi tsfn.
//                   Used during user-initiated "type mode" only. Issue #225
//                   Phase 2.
//   * `raw_input` — passive RegisterRawInputDevices observer with
//                   RIDEV_INPUTSINK that observes (does not swallow) global
//                   keystrokes. Used for the "user is typing in browser →
//                   nudge them with a hotkey hint" UX. Issue #225 Phase 3.
//
// On non-Windows targets the napi exports return errors so the binding still
// links cross-platform. macOS already has a separate stealth wiring path that
// doesn't suffer from issue #225, so there's nothing to implement here.

use napi::threadsafe_function::ThreadsafeFunction;

#[cfg(target_os = "windows")]
mod win_hook;

#[cfg(target_os = "windows")]
mod raw_input;

/// Single keystroke event delivered to JS. Mirrors the relevant fields from
/// KBDLLHOOKSTRUCT plus a resolved `character` (None for non-printable keys
/// like Esc, F-keys, arrows, modifiers in isolation, etc.).
///
/// The JS side is the gatekeeper: it decides which characters become input,
/// which are command keys (Enter, Esc, Backspace), and when to uninstall
/// the hook. The native layer just reports.
#[napi(object)]
pub struct KeyEvent {
    pub vk: u32,
    pub scancode: u32,
    pub down: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
    pub character: Option<String>,
}

// =========================================================================
// PHASE 2 — opt-in low-level keyboard hook (swallows keys)
// =========================================================================

#[cfg(target_os = "windows")]
#[napi]
pub fn install_keyboard_hook(callback: ThreadsafeFunction<KeyEvent>) -> napi::Result<()> {
    win_hook::install(callback)
}

#[cfg(not(target_os = "windows"))]
#[napi]
pub fn install_keyboard_hook(_callback: ThreadsafeFunction<KeyEvent>) -> napi::Result<()> {
    Err(napi::Error::from_reason(
        "install_keyboard_hook is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
#[napi]
pub fn uninstall_keyboard_hook() -> napi::Result<()> {
    win_hook::uninstall()
}

#[cfg(not(target_os = "windows"))]
#[napi]
pub fn uninstall_keyboard_hook() -> napi::Result<()> {
    Ok(())
}

// =========================================================================
// PHASE 3 — passive raw-input observer (does NOT swallow keys)
// =========================================================================

#[cfg(target_os = "windows")]
#[napi]
pub fn start_raw_input_observer(callback: ThreadsafeFunction<KeyEvent>) -> napi::Result<()> {
    raw_input::start(callback)
}

#[cfg(not(target_os = "windows"))]
#[napi]
pub fn start_raw_input_observer(_callback: ThreadsafeFunction<KeyEvent>) -> napi::Result<()> {
    Err(napi::Error::from_reason(
        "start_raw_input_observer is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
#[napi]
pub fn stop_raw_input_observer() -> napi::Result<()> {
    raw_input::stop()
}

#[cfg(not(target_os = "windows"))]
#[napi]
pub fn stop_raw_input_observer() -> napi::Result<()> {
    Ok(())
}
