/**
 * Screen order for the first-run onboarding. The component owns every side
 * effect; the rules that decide which screen follows which live here so they
 * can be checked without a DOM.
 *
 * `sync-account` is the RisuAI account backup and `sync-hub` is the RisuNest
 * sync server. The design document calls them `sync-server` and `sync-hub`;
 * the names here say which server each one means.
 */

import type { PeerCloneState } from 'src/ts/storage/sync/peerClone'

export const ONBOARDING_STATES = [
    'home',
    'import',
    'sync',
    'sync-device',
    'sync-hub',
    'sync-progress',
    'sync-account',
    'sync-account-found',
    'done',
] as const

export type OnboardingState = (typeof ONBOARDING_STATES)[number]

/** How the reader got their data. It decides the wording on the last screen. */
export type OnboardingPath = 'fresh' | 'import' | 'device' | 'hub' | 'account'

export interface OnboardingFlow {
    readonly state: OnboardingState
    readonly path: OnboardingPath
}

export const INITIAL_ONBOARDING_FLOW: OnboardingFlow = { state: 'home', path: 'fresh' }

/** 1 how to start, 2 data, 3 finished. */
export type OnboardingStep = 1 | 2 | 3

const STEP_OF: Readonly<Record<OnboardingState, OnboardingStep>> = {
    'home': 1,
    'import': 2,
    'sync': 2,
    'sync-device': 2,
    'sync-hub': 2,
    'sync-progress': 2,
    'sync-account': 2,
    'sync-account-found': 2,
    'done': 3,
}

/** The screen each back link returns to. `null` means the screen has none. */
const BACK_OF: Readonly<Record<OnboardingState, OnboardingState | null>> = {
    'home': null,
    'import': 'home',
    'sync': 'home',
    'sync-device': 'sync',
    'sync-hub': 'sync',
    // Leaving a running download is a cancellation, so the component asks
    // before it uses this target.
    'sync-progress': 'sync-device',
    'sync-account': 'sync',
    'sync-account-found': 'sync',
    'done': null,
}

/** The path each screen commits the reader to, where the screen decides one. */
const PATH_OF: Readonly<Partial<Record<OnboardingState, OnboardingPath>>> = {
    'home': 'fresh',
    'import': 'import',
    'sync-device': 'device',
    'sync-hub': 'hub',
    'sync-account': 'account',
    'sync-account-found': 'account',
}

export function onboardingStep(state: OnboardingState): OnboardingStep {
    return STEP_OF[state]
}

export function onboardingBack(state: OnboardingState): OnboardingState | null {
    return BACK_OF[state]
}

/**
 * Moves to `state`, carrying the path forward. A caller that knows better than
 * the screen it is entering can name the path itself.
 */
export function goToOnboardingState(
    flow: OnboardingFlow,
    state: OnboardingState,
    path?: OnboardingPath,
): OnboardingFlow {
    const next = path ?? PATH_OF[state] ?? flow.path
    // A download only ever runs for a peer or the sync server, so a path that
    // never reaches one would label the progress screen with the wrong source.
    if (state === 'sync-progress' && next !== 'device' && next !== 'hub') {
        return { state, path: 'device' }
    }
    return { state, path: next }
}

/** Which closing sentence the last screen shows. */
export function onboardingSummary(path: OnboardingPath): 'fresh' | 'import' | 'device' | 'data' {
    if (path === 'fresh' || path === 'import' || path === 'device') return path
    return 'data'
}

export type OnboardingClonePhase = PeerCloneState['target']['phase']

/**
 * Where the download screen goes once the clone reports how it ended.
 * Starting a download resolves at once; the transfer runs natively and only
 * the snapshot says whether it finished, failed, or was cancelled.
 */
export function onboardingCloneNext(phase: OnboardingClonePhase | undefined): OnboardingState | null {
    if (phase === 'completed') return 'done'
    if (phase === 'failed' || phase === 'cancelled') return 'sync-device'
    return null
}
