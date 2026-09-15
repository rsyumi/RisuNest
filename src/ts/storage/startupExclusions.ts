/**
 * What a start may leave switched off. The vocabulary lives on its own so the device settings
 * can name it without pulling in the recovery shell, which talks to the native side.
 */
export const RECOVERY_EXCLUSIONS = [
    'plugins',
    'modules',
    'regex',
    'theme',
    'sync',
    'autoUpdate',
    'account',
] as const

export type RecoveryExclusion = (typeof RECOVERY_EXCLUSIONS)[number]
