// Companion to useTypeMode for the passive raw-input observer (issue #225
// Phase 3). When the user starts typing in another window (e.g. their
// browser), the main process forwards a stream of 'raw-input-typing'
// events. This hook collapses that stream into a single boolean — "user
// is actively typing right now" — that the overlay can use to surface a
// faint nudge banner.
//
// The banner copy in NativelyInterface tells the user about the
// type-mode hotkey, so they can opt into capturing the next sentence
// instead of typing into the browser.

import { useEffect, useState } from 'react';

interface RawInputTypingInfo {
  vk: number;
  ts: number;
}

export interface UseRawInputNudgeOptions {
  /**
   * How long after the last keystroke the nudge stays visible. Tuned to
   * 1500ms — long enough that a paused typist doesn't see flicker, short
   * enough that the banner doesn't linger after the user stops.
   */
  fadeAfterMs?: number;
  /**
   * If true (default), the hook is a no-op once type mode is active. The
   * nudge is irrelevant when the user is already typing through Natively.
   */
  suppressDuringTypeMode?: boolean;
  /** Pass useTypeMode().active here to enable the suppression check. */
  typeModeActive?: boolean;
}

export function useRawInputNudge({
  fadeAfterMs = 1500,
  suppressDuringTypeMode = true,
  typeModeActive = false,
}: UseRawInputNudgeOptions = {}): boolean {
  const [showNudge, setShowNudge] = useState(false);

  useEffect(() => {
    const api = (window as any).electronAPI;
    if (!api?.onRawInputTyping) return;

    let lastKeystrokeAt = 0;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const off = api.onRawInputTyping((_info: RawInputTypingInfo) => {
      lastKeystrokeAt = Date.now();
      if (suppressDuringTypeMode && typeModeActive) return;
      setShowNudge(true);
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        // Re-check elapsed time at fire — if a newer keystroke landed in
        // the meantime, a later setTimeout will own the hide.
        if (Date.now() - lastKeystrokeAt >= fadeAfterMs) {
          setShowNudge(false);
        }
      }, fadeAfterMs);
    });

    return () => {
      try { off?.(); } catch {}
      if (timer) clearTimeout(timer);
    };
  }, [fadeAfterMs, suppressDuringTypeMode, typeModeActive]);

  // Force-hide whenever type mode flips on — without this, an in-flight
  // timer could leave the nudge briefly visible during the transition.
  useEffect(() => {
    if (suppressDuringTypeMode && typeModeActive && showNudge) {
      setShowNudge(false);
    }
  }, [suppressDuringTypeMode, typeModeActive, showNudge]);

  return showNudge;
}
