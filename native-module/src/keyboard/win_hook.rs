// WH_KEYBOARD_LL low-level keyboard hook on a dedicated message-pump thread.
//
// Lifecycle:
//   1. JS calls install_keyboard_hook(tsfn).
//   2. We spawn a dedicated thread, which:
//        a. Calls SetWindowsHookExW(WH_KEYBOARD_LL, ...).
//        b. Pumps GetMessageW until it receives WM_QUIT.
//        c. Calls UnhookWindowsHookEx and exits.
//      The thread reports back via an mpsc oneshot whether SetWindowsHookExW
//      succeeded, so install_keyboard_hook returns the right Result.
//   3. The hook proc is invoked by the OS on this thread for every keystroke.
//      It updates modifier-state atomics, resolves the character via
//      ToUnicode, builds a KeyEvent, and dispatches to JS via the tsfn.
//      It returns LRESULT(1) to swallow the keystroke (browser does not
//      receive it).
//   4. JS calls uninstall_keyboard_hook(), which PostThreadMessage(WM_QUIT)
//      to the hook thread. GetMessageW returns 0, the loop exits, the hook
//      is removed, and the thread joins.
//
// Constraints encoded here:
//   * Hook proc MUST return within LowLevelHooksTimeout (default 300ms) or
//     Windows silently unhooks us. tsfn dispatch is NonBlocking; no locks
//     are held across the dispatch.
//   * Hook proc must not panic. We wrap the body in catch_unwind and fall
//     back to CallNextHookEx on panic so the OS does not penalise us.
//   * Caller (JS) is the gatekeeper: this hook is installed only while the
//     user is in type mode, max ~5s typical idle window. No always-on
//     surveillance.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ToUnicode, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

use super::KeyEvent;

// Singleton state. Only one hook can be installed at a time (we'd rather
// reject a double-install than leak threads). Cleared in uninstall().
static RUNNING: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

// Modifier state, maintained from observed WM_KEYDOWN/UP transitions. We do
// NOT call GetKeyboardState in the hook proc — it returns the calling
// thread's keyboard state, not the system's, and inside an LL hook that's
// the hook thread's idle state (all zeros). Tracking from events is the
// canonical way to reflect actual modifier-down state.
static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static ALT_DOWN: AtomicBool = AtomicBool::new(false);
static WIN_DOWN: AtomicBool = AtomicBool::new(false);

// The tsfn lives for the lifetime of one install/uninstall cycle. Stored as
// a raw pointer in an AtomicUsize (machine-word-sized, matches *mut T width
// on every supported target — including 32-bit Windows where AtomicI64
// would waste 32 bits per access). Cleared in uninstall(); access is
// guarded by RUNNING.
static TSFN_PTR: AtomicUsize = AtomicUsize::new(0);

/// Owns the boxed tsfn so we can drop it on uninstall.
fn store_tsfn(tsfn: ThreadsafeFunction<KeyEvent>) {
    let raw = Box::into_raw(Box::new(tsfn)) as usize;
    let prev = TSFN_PTR.swap(raw, Ordering::AcqRel);
    if prev != 0 {
        // Drop any leftover from a prior cycle (defensive; shouldn't happen).
        unsafe {
            drop(Box::from_raw(prev as *mut ThreadsafeFunction<KeyEvent>));
        }
    }
}

fn take_tsfn() -> Option<Box<ThreadsafeFunction<KeyEvent>>> {
    let raw = TSFN_PTR.swap(0, Ordering::AcqRel);
    if raw == 0 {
        return None;
    }
    Some(unsafe { Box::from_raw(raw as *mut ThreadsafeFunction<KeyEvent>) })
}

/// Borrow the tsfn for the duration of a hook-proc call. No-op if uninstall
/// has already cleared it (race between WM_QUIT delivery and an in-flight
/// key event).
fn with_tsfn<F: FnOnce(&ThreadsafeFunction<KeyEvent>)>(f: F) {
    let raw = TSFN_PTR.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let tsfn = unsafe { &*(raw as *const ThreadsafeFunction<KeyEvent>) };
    f(tsfn);
}

pub fn install(tsfn: ThreadsafeFunction<KeyEvent>) -> napi::Result<()> {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return Err(napi::Error::from_reason(
            "keyboard hook is already installed",
        ));
    }

    // Reset modifier tracking — stale state from a previous cycle would
    // bleed into the next install if e.g. Shift was held during uninstall.
    //
    // KNOWN LIMITATION (issue #225): if the user is *currently holding*
    // a modifier when type mode starts (e.g. presses Ctrl+Shift+Space
    // with Shift still down on the next character), the first key event
    // will see SHIFT_DOWN=false because we have no synthetic key-down
    // history for it yet. The very next genuine WM_KEYDOWN for that
    // modifier corrects the state. v2 fix: prime each modifier from
    // GetAsyncKeyState() at install time — but that needs care to avoid
    // the LowLevelHooksTimeout budget.
    SHIFT_DOWN.store(false, Ordering::Release);
    CTRL_DOWN.store(false, Ordering::Release);
    ALT_DOWN.store(false, Ordering::Release);
    WIN_DOWN.store(false, Ordering::Release);

    store_tsfn(tsfn);

    // Type-pin the ready channel via an explicit let binding. A bare
    // `mpsc::channel::<Result<u32, String>>()` turbofish is technically
    // sufficient, but downstream usage (`Err(napi::Error::from_reason(msg))`)
    // can re-infer the Err type as `napi::Error<String>` and silently break
    // the send-side closure, so we pin both ends of the channel here.
    type ReadySignal = std::result::Result<u32, String>;
    let (ready_tx, ready_rx): (mpsc::Sender<ReadySignal>, mpsc::Receiver<ReadySignal>) =
        mpsc::channel();

    thread::spawn(move || {
        unsafe {
            let hinst: HINSTANCE = match GetModuleHandleW(None) {
                Ok(h) => HINSTANCE(h.0),
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("GetModuleHandleW failed: {}", e)));
                    RUNNING.store(false, Ordering::Release);
                    let _ = take_tsfn();
                    return;
                }
            };

            let hook: HHOOK = match SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(low_level_keyboard_proc),
                hinst,
                0,
            ) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("SetWindowsHookExW failed: {}", e)));
                    RUNNING.store(false, Ordering::Release);
                    let _ = take_tsfn();
                    return;
                }
            };

            let tid = GetCurrentThreadId();
            HOOK_THREAD_ID.store(tid, Ordering::Release);
            let _ = ready_tx.send(Ok(tid));

            // Pump messages until WM_QUIT. We don't need TranslateMessage /
            // DispatchMessage — we only need GetMessageW to keep returning so
            // the OS has a chance to invoke the hook proc on this thread.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // Drain. Hook proc is invoked synchronously by the OS; nothing
                // to do for the messages we receive.
            }

            let _ = UnhookWindowsHookEx(hook);
            HOOK_THREAD_ID.store(0, Ordering::Release);
            let _ = take_tsfn();
            RUNNING.store(false, Ordering::Release);
        }
    });

    match ready_rx.recv() {
        Ok(Ok(_tid)) => Ok(()),
        Ok(Err(msg)) => Err(napi::Error::from_reason(msg)),
        Err(e) => {
            RUNNING.store(false, Ordering::Release);
            let _ = take_tsfn();
            Err(napi::Error::from_reason(format!(
                "hook thread terminated before reporting status: {}",
                e
            )))
        }
    }
}

pub fn uninstall() -> napi::Result<()> {
    if !RUNNING.load(Ordering::Acquire) {
        return Ok(());
    }
    let tid = HOOK_THREAD_ID.load(Ordering::Acquire);
    if tid == 0 {
        return Ok(());
    }
    unsafe {
        // Posting WM_QUIT to the hook thread breaks GetMessageW out of its
        // loop, after which the thread runs UnhookWindowsHookEx and exits.
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
    Ok(())
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The OS may invoke the hook proc with code < 0 to signal "must call
    // next hook"; honour that contract.
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // Catch any panic so we don't poison the OS hook chain. We always fall
    // back to CallNextHookEx on panic; swallowing the key on panic would be
    // worse (user would lose the keystroke).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_key(code, wparam, lparam)
    }));

    match result {
        Ok(lr) => lr,
        Err(_) => CallNextHookEx(None, code, wparam, lparam),
    }
}

unsafe fn process_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let kb_ptr = lparam.0 as *const KBDLLHOOKSTRUCT;
    if kb_ptr.is_null() {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let kb = *kb_ptr;

    let event_kind = wparam.0 as u32;
    let down = matches!(event_kind, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(event_kind, WM_KEYUP | WM_SYSKEYUP);

    // Update modifier state from observed transitions.
    let vk = kb.vkCode;
    let is_modifier = update_modifier_state(vk, down, up);

    let alt = ALT_DOWN.load(Ordering::Acquire);
    let ctrl = CTRL_DOWN.load(Ordering::Acquire);
    let shift = SHIFT_DOWN.load(Ordering::Acquire);
    let win = WIN_DOWN.load(Ordering::Acquire);

    // Resolve printable character on key-down only. Key-up events with a
    // character would produce duplicate input on the JS side.
    let character = if down && !is_modifier {
        resolve_character(vk, kb.scanCode, shift, ctrl, alt)
    } else {
        None
    };

    with_tsfn(|tsfn| {
        let event = KeyEvent {
            vk,
            scancode: kb.scanCode,
            down,
            alt,
            ctrl,
            shift,
            win,
            character,
        };
        // NonBlocking: never wait on V8. If the JS event loop is saturated,
        // we'd rather drop a keystroke than blow past LowLevelHooksTimeout
        // and get the hook silently uninstalled.
        let _ = tsfn.call(Ok(event), ThreadsafeFunctionCallMode::NonBlocking);
    });

    // Swallow the keystroke. The browser does not see it. Caveat: while the
    // hook is installed the user cannot type into ANYTHING else (system
    // shortcuts included). The JS layer is responsible for installing this
    // hook only during type mode and uninstalling promptly on Esc / Enter
    // / idle timeout.
    LRESULT(1)
}

/// Returns true if the key was a modifier (Shift/Ctrl/Alt/Win), so callers
/// can suppress per-key character resolution for it.
unsafe fn update_modifier_state(vk: u32, down: bool, up: bool) -> bool {
    let lshift = VK_LSHIFT.0 as u32;
    let rshift = VK_RSHIFT.0 as u32;
    let shift = VK_SHIFT.0 as u32;
    let lctrl = VK_LCONTROL.0 as u32;
    let rctrl = VK_RCONTROL.0 as u32;
    let ctrl = VK_CONTROL.0 as u32;
    let lmenu = VK_LMENU.0 as u32;
    let rmenu = VK_RMENU.0 as u32;
    let menu = VK_MENU.0 as u32;
    let lwin = VK_LWIN.0 as u32;
    let rwin = VK_RWIN.0 as u32;

    if vk == lshift || vk == rshift || vk == shift {
        if down {
            SHIFT_DOWN.store(true, Ordering::Release);
        }
        if up {
            SHIFT_DOWN.store(false, Ordering::Release);
        }
        return true;
    }
    if vk == lctrl || vk == rctrl || vk == ctrl {
        if down {
            CTRL_DOWN.store(true, Ordering::Release);
        }
        if up {
            CTRL_DOWN.store(false, Ordering::Release);
        }
        return true;
    }
    if vk == lmenu || vk == rmenu || vk == menu {
        if down {
            ALT_DOWN.store(true, Ordering::Release);
        }
        if up {
            ALT_DOWN.store(false, Ordering::Release);
        }
        return true;
    }
    if vk == lwin || vk == rwin {
        if down {
            WIN_DOWN.store(true, Ordering::Release);
        }
        if up {
            WIN_DOWN.store(false, Ordering::Release);
        }
        return true;
    }
    false
}

/// Translate a vk + scancode + modifier snapshot into a printable string.
/// Returns None if the key has no printable representation (Esc, F-keys,
/// arrows, Backspace, etc.) — JS distinguishes those by looking at vk.
///
/// KNOWN LIMITATION — IME and dead-key keyboard layouts:
///   * `ToUnicode` MUTATES the kernel's per-thread dead-key state. For
///     dead-key layouts (most European: French, German, Spanish, etc.),
///     this hook will corrupt the dead-key buffer for OTHER apps that
///     compose with the same thread. Users in those locales may notice
///     stale accent / umlaut state after a type-mode session ends.
///   * For IME-using languages (CJK), this hook bypasses the IME
///     entirely — `ToUnicode` returns the bare romaji/pinyin character
///     instead of the composed glyph. Users on those locales should fall
///     back to STT until v2 wires `ImmGetContext` + `WM_IME_*` here.
///
/// Both limitations are surfaced in windowswork.md §10. v2 fix: switch
/// to `ToUnicodeEx(... wflags = 4)` (no dead-key state mutation) plus
/// IME composition pass-through. The v1 trade-off is intentional —
/// keeping the hook proc fast enough to stay under
/// LowLevelHooksTimeout matters more than IME perfection.
unsafe fn resolve_character(vk: u32, scancode: u32, shift: bool, ctrl: bool, alt: bool) -> Option<String> {
    // Synthesize a keyboard-state buffer reflecting the current modifier
    // snapshot. ToUnicode reads bit 0x80 ("key is down") on each VK index.
    let mut state = [0u8; 256];
    if shift {
        state[VK_SHIFT.0 as usize] = 0x80;
    }
    if ctrl {
        state[VK_CONTROL.0 as usize] = 0x80;
    }
    if alt {
        state[VK_MENU.0 as usize] = 0x80;
    }

    // If Ctrl is held without Alt, ToUnicode produces control characters
    // (Ctrl+A → 0x01, etc.) — we don't want those in the input field. The
    // JS side handles command-key combos directly via the vk field.
    if ctrl && !alt {
        return None;
    }

    let mut buf = [0u16; 8];
    // wflags = 0: don't change kernel keyboard state. We can't fully avoid
    // dead-key state mutation, but flags=0 is the standard caller pattern
    // for non-IME consumption.
    let n = ToUnicode(vk, scancode, Some(&state), &mut buf, 0);
    if n <= 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..n as usize]);
    if s.is_empty() {
        return None;
    }
    Some(s)
}
