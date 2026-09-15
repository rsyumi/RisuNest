/**
 * The recovery shell's state. `loadData()` never runs while this is armed, so nothing here may
 * read `DBState`, a plugin, a module or the sync engine: the whole point is that none of them
 * were initialised.
 */
import { invoke } from '@tauri-apps/api/core'
import {
    beginBootAttempt,
    bootModeFor,
    completeBootAttempt,
    type BootAttemptBridge,
    type BootDecision,
    type BootMode,
    type BootTrail,
} from './bootAttempt'
import {
    RECOVERY_EXCLUSIONS,
    type RecoveryExclusion,
} from './startupExclusions'

export {
    RECOVERY_EXCLUSIONS,
    type RecoveryExclusion,
} from './startupExclusions'

export interface RecoveryState {
    mode: BootMode
    decision: BootDecision | null
    trail: BootTrail
    /** What the shell currently has switched off. */
    excluded: RecoveryExclusion[]
    /** What the ordinary start must honour for this run, once the shell hands over. */
    applied: RecoveryExclusion[]
}

const nativeBridge: BootAttemptBridge = {
    begin: () => invoke('boot_attempt_begin'),
    complete: async () => {
        await invoke('boot_attempt_complete')
    },
}

const state = $state<RecoveryState>({
    mode: 'normal',
    decision: null,
    trail: { stage: null, suspect: null },
    excluded: [],
    applied: [],
})

export function recoveryState(): RecoveryState {
    return state
}

/** True while the recovery shell is on screen, which is exactly while `loadData()` must not run. */
export function isRecoveryShellOpen(): boolean {
    return state.mode !== 'normal'
}

/** What this run leaves switched off. Empty on an ordinary start. */
export function recoveryExclusions(): readonly RecoveryExclusion[] {
    return state.applied
}

export function isExcluded(exclusion: RecoveryExclusion): boolean {
    return recoveryExclusions().includes(exclusion)
}

/** Plugins a suspect trail named are switched off to begin with, and the reader can change that. */
export function suspectedExclusions(trail: BootTrail): RecoveryExclusion[] {
    const suspected: RecoveryExclusion[] = []
    if (trail.suspect?.startsWith('plugin:') || trail.stage === 'plugins')
        suspected.push('plugins')
    if (trail.stage === 'account-bootstrap' || trail.stage === 'account-data')
        suspected.push('account')
    if (trail.stage === 'drive-sync') suspected.push('sync')
    if (trail.stage === 'ui-state') suspected.push('theme')
    return suspected
}

/**
 * Reads the last start's outcome and decides what this one does. Called before anything the
 * start could fail in, so a start that never reaches the app still counts as one.
 */
export async function decideBoot(
    bridge: BootAttemptBridge = nativeBridge,
): Promise<BootMode> {
    const { decision, trail } = await beginBootAttempt(bridge)
    state.decision = decision
    state.trail = trail
    state.mode = bootModeFor(decision)
    state.excluded = state.mode === 'normal' ? [] : suspectedExclusions(trail)
    state.applied = []
    return state.mode
}

/** Records that this start finished, so the next one begins from zero. */
export function finishBoot(bridge: BootAttemptBridge = nativeBridge): Promise<void> {
    return completeBootAttempt(bridge)
}

export function toggleExclusion(exclusion: RecoveryExclusion): void {
    state.excluded = state.excluded.includes(exclusion)
        ? state.excluded.filter((item) => item !== exclusion)
        : [...state.excluded, exclusion]
}

/**
 * Leaves the shell and lets the ordinary start run with the current exclusions. The exclusions
 * stay in memory: the shell never writes to the library.
 */
export function startNormally(): readonly RecoveryExclusion[] {
    // The list is what the ordinary start must honour, so it outlives the shell for this run.
    state.applied = state.excluded
    state.mode = 'normal'
    return state.applied
}

/**
 * Whether a feature stays off for this start: either the recovery shell switched it off for this
 * run, or the reader confirmed keeping it off on this device.
 */
export function isStartupExcluded(
    exclusion: RecoveryExclusion,
    persisted: readonly RecoveryExclusion[] = [],
): boolean {
    return recoveryExclusions().includes(exclusion) || persisted.includes(exclusion)
}

/**
 * Offers to keep this run's exclusions after a start that actually finished. Nothing is written
 * unless the reader says so, so the next start comes back with everything on by default.
 */
export async function confirmRecoveryExclusions(
    excluded: readonly RecoveryExclusion[],
    ask: (message: string) => Promise<boolean>,
    persist: (exclusions: RecoveryExclusion[]) => void,
    describe: (exclusion: RecoveryExclusion) => string,
    message: string,
): Promise<boolean> {
    if (excluded.length === 0) return false
    const named = excluded.map(describe).join(', ')
    if (!(await ask(message.replace('{0}', named)))) return false
    persist([...excluded])
    return true
}
