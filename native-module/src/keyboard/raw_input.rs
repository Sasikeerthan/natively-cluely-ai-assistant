// Passive raw-input observer — Phase 3 of windowswork.md (issue #225).
//
// Distinct from the LL keyboard hook in win_hook.rs:
//
//   * win_hook  — opt-in, swallows keys, used during type mode
//   * raw_input — always-on (or as opted-in), observes only, never swallows
//
// Use case: detect when the user starts typing in their browser so the
// overlay can show a faint "press Ctrl+Shift+Space to ask Natively"
// nudge. Because we don't swallow, the browser still receives every key,
// so this carries far less AV / EDR risk than the LL hook.
//
// Implementation:
//   1. A dedicated thread creates a HWND_MESSAGE (message-only) window.
//      This window is not visible and will never appear on the desktop;
//      its only purpose is to receive WM_INPUT messages.
//   2. RegisterRawInputDevices subscribes to keyboard input with the
//      RIDEV_INPUTSINK flag, which delivers events even when our window
//      is not in the foreground.
//   3. A standard message-pump loop dispatches WM_INPUT to the window
//      proc, which calls GetRawInputData and forwards a KeyEvent over
//      the napi tsfn.
//   4. stop() PostMessage(WM_CLOSE) the window; the window proc handles
//      WM_DESTROY by PostQuitMessage; the loop exits.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, CW_USEDEFAULT, HMENU, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_DESTROY, WM_INPUT, WNDCLASSEXW,
};

use super::KeyEvent;

static RUNNING: AtomicBool = AtomicBool::new(false);
static OBSERVER_HWND: AtomicIsize = AtomicIsize::new(0);
static OBSERVER_THREAD_ID: AtomicU32 = AtomicU32::new(0);

static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static ALT_DOWN: AtomicBool = AtomicBool::new(false);
static WIN_DOWN: AtomicBool = AtomicBool::new(false);

// Pointer-width atomic for the boxed tsfn — see win_hook.rs for the
// reasoning. Stored across install/uninstall cycles, guarded by RUNNING.
static TSFN_PTR: AtomicUsize = AtomicUsize::new(0);

fn store_tsfn(tsfn: ThreadsafeFunction<KeyEvent>) {
    let raw = Box::into_raw(Box::new(tsfn)) as usize;
    let prev = TSFN_PTR.swap(raw, Ordering::AcqRel);
    if prev != 0 {
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

fn with_tsfn<F: FnOnce(&ThreadsafeFunction<KeyEvent>)>(f: F) {
    let raw = TSFN_PTR.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let tsfn = unsafe { &*(raw as *const ThreadsafeFunction<KeyEvent>) };
    f(tsfn);
}

const WINDOW_CLASS_NAME: &[u16] = &[
    'N' as u16, 'a' as u16, 't' as u16, 'i' as u16, 'v' as u16, 'e' as u16, 'l' as u16, 'y' as u16,
    'R' as u16, 'a' as u16, 'w' as u16, 'I' as u16, 'n' as u16, 'p' as u16, 'u' as u16, 't' as u16,
    0,
];

pub fn start(tsfn: ThreadsafeFunction<KeyEvent>) -> napi::Result<()> {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return Err(napi::Error::from_reason(
            "raw input observer is already running",
        ));
    }

    SHIFT_DOWN.store(false, Ordering::Release);
    CTRL_DOWN.store(false, Ordering::Release);
    ALT_DOWN.store(false, Ordering::Release);
    WIN_DOWN.store(false, Ordering::Release);

    store_tsfn(tsfn);

    // See win_hook.rs for the rationale behind type-pinning the channel.
    type ReadySignal = std::result::Result<(), String>;
    let (ready_tx, ready_rx): (mpsc::Sender<ReadySignal>, mpsc::Receiver<ReadySignal>) =
        mpsc::channel();

    thread::spawn(move || {
        unsafe {
            let hinst = match GetModuleHandleW(None) {
                Ok(h) => HINSTANCE(h.0),
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("GetModuleHandleW failed: {}", e)));
                    cleanup();
                    return;
                }
            };

            // Register the message-only window class. We don't bother
            // checking for the "class already exists" error because each
            // process should only get here once at most per session, and
            // re-registering with the same atom is harmless on Windows.
            let mut wc = WNDCLASSEXW::default();
            wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            wc.lpfnWndProc = Some(window_proc);
            wc.hInstance = hinst;
            wc.lpszClassName = PCWSTR(WINDOW_CLASS_NAME.as_ptr());
            let _atom = RegisterClassExW(&wc);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(WINDOW_CLASS_NAME.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                HWND_MESSAGE,
                HMENU::default(),
                hinst,
                None,
            );
            if hwnd.0 == 0 {
                let _ = ready_tx.send(Err("CreateWindowExW failed for HWND_MESSAGE".into()));
                cleanup();
                return;
            }

            // Subscribe to keyboard raw input. usage page 0x01 + usage
            // 0x06 = generic keyboard. RIDEV_INPUTSINK lets us receive
            // events while another window has foreground (the whole
            // point of the observer).
            let device = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            // In windows-rs 0.52 RegisterRawInputDevices returns
            // Result<()>; in older versions it was BOOL with .as_bool().
            // Handle the Result form here.
            if let Err(e) = RegisterRawInputDevices(
                &[device],
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            ) {
                let _ = DestroyWindow(hwnd);
                let _ = ready_tx
                    .send(Err(format!("RegisterRawInputDevices failed: {}", e)));
                cleanup();
                return;
            }

            OBSERVER_HWND.store(hwnd.0 as isize, Ordering::Release);
            OBSERVER_THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);
            let _ = ready_tx.send(Ok(()));

            // The pump only services WM_INPUT (and the WM_CLOSE/WM_DESTROY
            // teardown sequence). TranslateMessage is meaningful only for
            // WM_KEYDOWN/WM_KEYUP -> WM_CHAR synthesis, which we never
            // observe — so omit it to save a syscall per message.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }

            // Unsubscribe and tear down. RIDEV_REMOVE requires
            // hwndTarget to be NULL per the API contract.
            let remove = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND(0),
            };
            let _ = RegisterRawInputDevices(
                &[remove],
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            );

            let _ = DestroyWindow(hwnd);
            cleanup();
        }
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(napi::Error::from_reason(msg)),
        Err(e) => {
            cleanup();
            Err(napi::Error::from_reason(format!(
                "raw input observer thread terminated before reporting status: {}",
                e
            )))
        }
    }
}

pub fn stop() -> napi::Result<()> {
    if !RUNNING.load(Ordering::Acquire) {
        return Ok(());
    }
    let raw = OBSERVER_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return Ok(());
    }
    unsafe {
        // WM_CLOSE → DefWindowProc → DestroyWindow → WM_DESTROY →
        // PostQuitMessage exits the pump. PostMessageW is thread-safe.
        let _ = PostMessageW(HWND(raw as isize), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    Ok(())
}

fn cleanup() {
    OBSERVER_HWND.store(0, Ordering::Release);
    OBSERVER_THREAD_ID.store(0, Ordering::Release);
    let _ = take_tsfn();
    RUNNING.store(false, Ordering::Release);
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_raw_input(lparam);
        }));
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn handle_raw_input(lparam: LPARAM) {
    let h_raw = HRAWINPUT(lparam.0);
    let mut raw: RAWINPUT = std::mem::zeroed();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;

    let n = GetRawInputData(
        h_raw,
        RID_INPUT,
        Some(&mut raw as *mut _ as *mut _),
        &mut size,
        header_size,
    );
    if n as i32 <= 0 {
        return;
    }
    if raw.header.dwType != RIM_TYPEKEYBOARD.0 {
        return;
    }

    let kb = raw.data.keyboard;
    let vk = kb.VKey as u32;
    // Flags bit 0 = key up. We treat key down as anything else.
    let down = (kb.Flags & 0x01) == 0;
    let up = !down;

    let _ = update_modifier_state(vk, down, up);

    let alt = ALT_DOWN.load(Ordering::Acquire);
    let ctrl = CTRL_DOWN.load(Ordering::Acquire);
    let shift = SHIFT_DOWN.load(Ordering::Acquire);
    let win = WIN_DOWN.load(Ordering::Acquire);

    // Observer-only: we don't resolve characters here. The JS side just
    // needs "user pressed something" + the vk to decide whether to nudge
    // (e.g. ignore pure modifier presses, ignore navigation keys). If a
    // future use case needs characters from the observer we can wire
    // ToUnicode in the same way win_hook.rs does it.
    with_tsfn(|tsfn| {
        let event = KeyEvent {
            vk,
            scancode: kb.MakeCode as u32,
            down,
            alt,
            ctrl,
            shift,
            win,
            character: None,
        };
        let _ = tsfn.call(Ok(event), ThreadsafeFunctionCallMode::NonBlocking);
    });
}

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
