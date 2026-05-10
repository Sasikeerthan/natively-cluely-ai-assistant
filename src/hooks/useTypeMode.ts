// React hook for the Win32 stealth type-mode pipeline (issue #225 Phase 2).
//
// While type mode is active, the main process installs a low-level keyboard
// hook that swallows every keystroke and forwards it to the overlay
// renderer over IPC. This hook:
//
//   * subscribes to those keystrokes,
//   * accumulates printable characters into an internal buffer,
//   * routes command keys (Enter / Esc / Backspace) to the right callbacks,
//   * mirrors the main-process type-mode active flag so the indicator UI
//     can re-sync after a hotkey toggle or a 5s idle auto-exit,
//   * exposes an `appendToInput` callback so consumers can drive an
//     existing controlled <input> without owning the full state machine.
//
// On non-Windows the hook is a noop: `active` stays false, no listeners
// register, nothing crashes.

import { useCallback, useEffect, useRef, useState } from 'react';
import type { KeyboardHookEvent } from '../types/keyboardEvent';

const VK_BACK = 0x08;
const VK_RETURN = 0x0D;
const VK_ESCAPE = 0x1B;

interface TypeModeStateEvent {
  active: boolean;
  /**
   * Why state changed:
   *   * undefined — explicit enterTypeMode/exitTypeMode call
   *   * 'idle' — auto-uninstalled after HOOK_IDLE_TIMEOUT_MS without keys
   *   * 'silently-uninstalled' — Windows revoked the LL hook because the
   *     proc exceeded LowLevelHooksTimeout. The user thinks they're typing
   *     into Natively but their keystrokes are reaching the underlying
   *     app. The renderer should show a clear error.
   */
  reason?: 'idle' | 'silently-uninstalled';
}

export interface UseTypeModeOptions {
  /**
   * Called for every printable character or supported command key. The
   * consumer applies it to whatever input field it owns.
   */
  onChar?: (ch: string) => void;
  onBackspace?: () => void;
  /**
   * Called when the user presses Enter inside type mode. The consumer
   * should submit whatever it accumulated and call `exit()` afterwards
   * (or do nothing — the main process also exits type mode on its own
   * idle timer).
   */
  onSubmit?: () => void;
  /**
   * Called when the user presses Esc. Default behaviour: just exit.
   */
  onCancel?: () => void;
  /**
   * If true, mounting the hook automatically queries the main process for
   * the current type-mode state. Useful when the hotkey can flip type
   * mode on before the renderer is ready to listen.
   */
  syncInitialState?: boolean;
  /**
   * Called when the LL hook was uninstalled by the OS without us asking.
   * Today the only known cause is `LowLevelHooksTimeout`. The renderer
   * should surface a toast / error banner — the user just typed into the
   * underlying app instead of into Natively.
   */
  onSilentlyUninstalled?: () => void;
}

export interface UseTypeModeResult {
  /** Mirrors the main process type-mode flag. */
  active: boolean;
  /** Last keystroke received — useful for blink/animation hooks. */
  lastEvent: KeyboardHookEvent | null;
  /** Manually start type mode (also wired to a Win32 global shortcut). */
  enter: () => Promise<{ ok: boolean; reason?: string }>;
  /** Manually end type mode. Idempotent. */
  exit: () => Promise<void>;
}

export function useTypeMode(opts: UseTypeModeOptions = {}): UseTypeModeResult {
  const [active, setActive] = useState(false);
  const [lastEvent, setLastEvent] = useState<KeyboardHookEvent | null>(null);

  // Stash the latest opts in a ref so the IPC subscription's effect can
  // remain mount-scoped (a clean unsubscribe-on-unmount) while always
  // dispatching to the freshest callbacks.
  const optsRef = useRef(opts);
  optsRef.current = opts;

  useEffect(() => {
    const api = (window as any).electronAPI;
    if (!api?.onTypeModeKey || !api?.onTypeModeStateChange) return;

    const offKey = api.onTypeModeKey((event: KeyboardHookEvent) => {
      setLastEvent(event);
      const o = optsRef.current;

      // Command keys first — these are dispatched even when modifiers are
      // held, so a user habit of "Shift+Enter" still submits.
      if (event.vk === VK_RETURN) {
        o.onSubmit?.();
        return;
      }
      if (event.vk === VK_ESCAPE) {
        o.onCancel?.();
        return;
      }
      if (event.vk === VK_BACK) {
        o.onBackspace?.();
        return;
      }

      if (event.character && event.character.length > 0) {
        o.onChar?.(event.character);
      }
    });

    const offState = api.onTypeModeStateChange((state: TypeModeStateEvent) => {
      setActive(state.active);
      if (!state.active && state.reason === 'silently-uninstalled') {
        optsRef.current.onSilentlyUninstalled?.();
      }
    });

    if (opts.syncInitialState && typeof api.isTypeModeActive === 'function') {
      api.isTypeModeActive().then((value: boolean) => setActive(!!value)).catch(() => {});
    }

    return () => {
      try { offKey?.(); } catch {}
      try { offState?.(); } catch {}
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const enter = useCallback(async () => {
    const api = (window as any).electronAPI;
    if (!api?.enterTypeMode) return { ok: false, reason: 'electronAPI.enterTypeMode missing' };
    try {
      const result = await api.enterTypeMode();
      if (result?.ok) setActive(true);
      return result ?? { ok: false, reason: 'no result' };
    } catch (e) {
      return { ok: false, reason: e instanceof Error ? e.message : String(e) };
    }
  }, []);

  const exit = useCallback(async () => {
    const api = (window as any).electronAPI;
    if (!api?.exitTypeMode) return;
    try {
      await api.exitTypeMode();
      setActive(false);
    } catch {
      // Best effort — exit is idempotent on the main side.
    }
  }, []);

  return { active, lastEvent, enter, exit };
}
