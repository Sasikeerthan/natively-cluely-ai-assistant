/**
 * Sentinel value stored in `nativelyApiKey` while a free trial is active.
 *
 * The trial token (`natively_trial_…`) is *not* a valid API key, but the
 * downstream code (LLMHelper, NativelyProSTT, ipcHandlers) needs to treat
 * "trial mode" identically to "key mode" for routing/auto-promotion. We store
 * this sentinel in CredentialsManager so the existing `if (nativelyApiKey)`
 * branches all light up, then swap the auth header to `x-trial-token` at the
 * actual network boundary.
 *
 * Any place that reads `nativelyApiKey` and forwards it to the network MUST
 * compare against TRIAL_SENTINEL_KEY (not the literal '__trial__') so a single
 * rename here updates every call site.
 */
export const TRIAL_SENTINEL_KEY = '__trial__' as const;

export const CRACKWITHAI_API_HTTP_BASE = 'https://api.crackwithai.com' as const;
export const CRACKWITHAI_API_WS_BASE = 'wss://api.crackwithai.com' as const;

export const CRACKWITHAI_API_ENDPOINTS = {
  chat: `${CRACKWITHAI_API_HTTP_BASE}/v1/chat`,
  transcribe: `${CRACKWITHAI_API_WS_BASE}/v1/transcribe`,
  usage: `${CRACKWITHAI_API_HTTP_BASE}/v1/usage`,
  trialStart: `${CRACKWITHAI_API_HTTP_BASE}/v1/trial/start`,
  trialStatus: `${CRACKWITHAI_API_HTTP_BASE}/v1/trial/status`,
  trialConvert: `${CRACKWITHAI_API_HTTP_BASE}/v1/trial/convert`,
} as const;
