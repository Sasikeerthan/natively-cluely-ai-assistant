/**
 * Centralised checkout & external URL constants.
 *
 * Change URLs here once — all files that import from this module
 * will pick up the update automatically.
 */

export const CHECKOUT_URLS = {
    /** Natively Pro (lifetime/yearly) */
    pro: 'https://beta.crackwithai.com',
    /** Natively API — Standard tier */
    apiStandard: 'https://beta.crackwithai.com',
    /** Natively API — Pro tier */
    apiPro: 'https://beta.crackwithai.com',
    /** Natively API — Max tier */
    apiMax: 'https://beta.crackwithai.com',
    /** Natively API — Ultra tier */
    apiUltra: 'https://beta.crackwithai.com',
} as const;
