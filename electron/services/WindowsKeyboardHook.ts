// TS bridge for the Windows-only stealth keyboard pipeline.
//
// Wraps two distinct native subsystems exposed by the Rust native module:
//
//   * WH_KEYBOARD_LL low-level hook — opt-in, swallows keys, used during
//     "type mode" so the user can type into the overlay without the
//     overlay's HWND becoming foreground (issue #225 Phase 2).
//
//   * Raw-input observer — passive, observes only, used to detect "user is
//     typing into their browser → nudge them toward the type-mode hotkey"
//     (issue #225 Phase 3).
//
// Both surfaces are no-ops on non-Windows. The Rust side returns errors
// there; we short-circuit before the native call so non-Windows callers
// don't see error logs.

import { loadNativeModule } from '../audio/nativeModuleLoader';
import type { KeyboardHookEvent } from '../../src/types/keyboardEvent';

// Re-export the canonical event type so existing call sites that imported
// NativeKeyEvent from here continue to compile. Source of truth lives in
// src/types/keyboardEvent.ts.
export type NativeKeyEvent = KeyboardHookEvent;

/**
 * Idle window after which the LL hook auto-uninstalls if no keys are
 * received. Defensive: guarantees the hook is never left installed if the
 * UI's Esc/Enter handler is somehow missed.
 *
 * Tuned to 5s per windowswork.md §4 — long enough to think between
 * keystrokes, short enough that an accidentally-orphaned hook self-heals
 * fast.
 */
const HOOK_IDLE_TIMEOUT_MS = 5_000;

/**
 * Wall-clock budget after install() during which we expect the OS to deliver
 * at least one key event (assuming the user is actually typing — they'd have
 * just pressed the type-mode hotkey). If nothing arrives within this window
 * we conclude Windows silently uninstalled the hook because the proc
 * exceeded LowLevelHooksTimeout (registry default 300ms; can be lower under
 * load) and surface that to the consumer so the UI can fall back gracefully
 * rather than silently swallowing the user's keystrokes from the browser.
 */
const HOOK_FIRST_EVENT_BUDGET_MS = 1_500;

export interface WindowsKeyboardHookEvents {
  /** Called for every key event received from the LL hook. */
  onKey: (event: NativeKeyEvent) => void;
  /**
   * Called when the hook auto-uninstalls due to the idle timer (i.e. the
   * user stopped typing). The renderer should clear its "typing..." badge.
   */
  onIdleTimeout?: () => void;
  /**
   * Called when no key event has arrived within HOOK_FIRST_EVENT_BUDGET_MS
   * of install. Strong signal that Windows silently uninstalled the hook
   * (LowLevelHooksTimeout). The consumer should treat type mode as
   * non-functional and surface a clear error state.
   */
  onSilentlyUninstalled?: () => void;
}

export class WindowsKeyboardHook {
  private active = false;
  private idleTimer: NodeJS.Timeout | null = null;
  private firstEventWatchdog: NodeJS.Timeout | null = null;
  private hasReceivedAnyKey = false;
  private lastError: Error | null = null;

  constructor(private events: WindowsKeyboardHookEvents) {}

  /**
   * Install the LL hook. The browser will stop seeing keystrokes until
   * stop() is called or the idle timer fires.
   *
   * Throws on Win32 if the native module is missing the symbol (binary
   * needs rebuild) or if SetWindowsHookExW fails.
   */
  start(): void {
    if (process.platform !== 'win32') return;
    if (this.active) return;

    const native = loadNativeModule();
    if (!native) {
      throw new Error('WindowsKeyboardHook: native module failed to load');
    }
    if (typeof native.installKeyboardHook !== 'function') {
      throw new Error(
        'WindowsKeyboardHook: native binary is missing installKeyboardHook ' +
        '— rebuild required (cargo build --release in native-module/)'
      );
    }

    native.installKeyboardHook((err, event) => {
      if (err) {
        // The Rust side never sends Err today, but the napi tsfn signature
        // allows it. Track and stop on error.
        this.lastError = err;
        console.error('[WindowsKeyboardHook] native error:', err);
        this.stop();
        return;
      }
      this.hasReceivedAnyKey = true;
      this.bumpIdleTimer();
      try {
        this.events.onKey(event);
      } catch (e) {
        console.error('[WindowsKeyboardHook] onKey listener threw:', e);
      }
    });

    this.active = true;
    this.hasReceivedAnyKey = false;
    this.bumpIdleTimer();
    this.armFirstEventWatchdog();
    console.log('[WindowsKeyboardHook] LL hook installed');
  }

  stop(): void {
    if (!this.active) return;
    this.active = false;
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
    if (this.firstEventWatchdog) {
      clearTimeout(this.firstEventWatchdog);
      this.firstEventWatchdog = null;
    }

    const native = loadNativeModule();
    if (native && typeof native.uninstallKeyboardHook === 'function') {
      try {
        native.uninstallKeyboardHook();
      } catch (e) {
        console.error('[WindowsKeyboardHook] uninstall failed:', e);
      }
    }
    console.log('[WindowsKeyboardHook] LL hook uninstalled');
  }

  isActive(): boolean {
    return this.active;
  }

  getLastError(): Error | null {
    return this.lastError;
  }

  private bumpIdleTimer(): void {
    if (this.idleTimer) clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => {
      console.log('[WindowsKeyboardHook] idle timeout — auto-uninstalling');
      this.stop();
      this.events.onIdleTimeout?.();
    }, HOOK_IDLE_TIMEOUT_MS);
  }

  /**
   * Detects the case where Windows silently uninstalled our hook because
   * the proc exceeded `LowLevelHooksTimeout`. The hook handle is technically
   * still tracked in our process but the OS no longer dispatches to it, so
   * the user types into the browser thinking type mode is active and gets
   * nothing back. We give the OS up to HOOK_FIRST_EVENT_BUDGET_MS to
   * deliver any key event; if none arrives, we tear down and notify.
   *
   * The watchdog only runs once per install — after the first event, it is
   * cancelled and never re-armed for that session. It is reset on each new
   * start().
   */
  private armFirstEventWatchdog(): void {
    if (this.firstEventWatchdog) clearTimeout(this.firstEventWatchdog);
    this.firstEventWatchdog = setTimeout(() => {
      this.firstEventWatchdog = null;
      if (!this.active || this.hasReceivedAnyKey) return;
      console.warn(
        `[WindowsKeyboardHook] no key events within ${HOOK_FIRST_EVENT_BUDGET_MS}ms ` +
        `— Windows likely silently uninstalled the hook (LowLevelHooksTimeout)`
      );
      this.stop();
      this.events.onSilentlyUninstalled?.();
    }, HOOK_FIRST_EVENT_BUDGET_MS);
  }
}

// =========================================================================
// Phase 3 — passive raw-input observer
// =========================================================================

export interface RawInputObserverEvents {
  /** Called for every keystroke seen anywhere on the system. Observe-only. */
  onKey: (event: NativeKeyEvent) => void;
}

export class WindowsRawInputObserver {
  private active = false;

  constructor(private events: RawInputObserverEvents) {}

  start(): void {
    if (process.platform !== 'win32') return;
    if (this.active) return;

    const native = loadNativeModule();
    if (!native) {
      throw new Error('WindowsRawInputObserver: native module failed to load');
    }
    if (typeof native.startRawInputObserver !== 'function') {
      throw new Error(
        'WindowsRawInputObserver: native binary is missing startRawInputObserver ' +
        '— rebuild required'
      );
    }

    native.startRawInputObserver((err, event) => {
      if (err) {
        console.error('[WindowsRawInputObserver] native error:', err);
        this.stop();
        return;
      }
      try {
        this.events.onKey(event);
      } catch (e) {
        console.error('[WindowsRawInputObserver] onKey listener threw:', e);
      }
    });
    this.active = true;
    console.log('[WindowsRawInputObserver] raw-input observer started');
  }

  stop(): void {
    if (!this.active) return;
    this.active = false;

    const native = loadNativeModule();
    if (native && typeof native.stopRawInputObserver === 'function') {
      try {
        native.stopRawInputObserver();
      } catch (e) {
        console.error('[WindowsRawInputObserver] stop failed:', e);
      }
    }
    console.log('[WindowsRawInputObserver] raw-input observer stopped');
  }

  isActive(): boolean {
    return this.active;
  }
}
