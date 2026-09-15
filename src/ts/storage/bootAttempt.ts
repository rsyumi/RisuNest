/**
 * Whether the last start finished. The native marker is the judgement; the stage and suspect the
 * renderer writes here are hints that help name what went wrong, and may be lost in a crash.
 */
import { isTauri } from '../platform'

export interface BootAttemptSummary {
    startedAt: number
    appVersion: string
    consecutiveFailures: number
}

export interface BootDecision {
    consecutiveFailures: number
    previous?: BootAttemptSummary
}

/** What the renderer does about a start that did not finish. */
export type BootMode = 'normal' | 'choose' | 'recovery'

/**
 * One failure offers the choice; a second opens the recovery shell without asking, because the
 * reader has already tried a normal start and it did not finish.
 */
export function bootModeFor(decision: BootDecision | null): BootMode {
    if (!decision || decision.consecutiveFailures === 0) return 'normal'
    return decision.consecutiveFailures === 1 ? 'choose' : 'recovery'
}

const STAGE_KEY = 'risunest-boot-stage'
const SUSPECT_KEY = 'risunest-boot-suspect'

/** The trail survives a crash only as far as the WebView flushed it, so it is never the gate. */
export interface BootTrail {
    stage: string | null
    suspect: string | null
}

function write(key: string, value: string | null): void {
    try {
        if (value === null) localStorage.removeItem(key)
        else localStorage.setItem(key, value)
    } catch {
        // A blocked store only costs the hint.
    }
}

function read(key: string): string | null {
    try {
        return localStorage.getItem(key)
    } catch {
        return null
    }
}

/** Records the stage a start has reached, synchronously, before the stage runs. */
export function markBootStage(stage: string): void {
    write(STAGE_KEY, stage)
}

/** Records what is being loaded right now, so a start that never returns names it. */
export function markBootSuspect(suspect: string | null): void {
    write(SUSPECT_KEY, suspect)
}

export function readBootTrail(): BootTrail {
    return { stage: read(STAGE_KEY), suspect: read(SUSPECT_KEY) }
}

export function clearBootTrail(): void {
    write(STAGE_KEY, null)
    write(SUSPECT_KEY, null)
}

export interface BootAttemptBridge {
    begin(): Promise<BootDecision>
    complete(): Promise<void>
}

/**
 * Reads the last attempt and records this one. A failure to reach the marker is not a reason to
 * refuse the start: the app opens normally and simply loses the judgement for this run.
 */
export async function beginBootAttempt(
    bridge: BootAttemptBridge,
): Promise<{ decision: BootDecision | null; trail: BootTrail }> {
    const trail = readBootTrail()
    clearBootTrail()
    if (!isTauri) return { decision: null, trail }
    try {
        return { decision: await bridge.begin(), trail }
    } catch {
        return { decision: null, trail }
    }
}

export async function completeBootAttempt(
    bridge: BootAttemptBridge,
): Promise<void> {
    clearBootTrail()
    if (!isTauri) return
    try {
        await bridge.complete()
    } catch {
        // The next start counts one more failure, which the choice screen can clear.
    }
}
