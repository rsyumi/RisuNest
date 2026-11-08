import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import type { Database } from '../../src/ts/storage/database.svelte'
import type { PersistentDataStore } from '../../src/ts/storage/persistentDataStore'
import { failureRecord } from '../../benchmarks/failureRecord'

interface Phase { runId: string; phase: string }

// These phases end through the app's own native exit, so each records its report first.
const nativeExitPhases = new Set(['close-escape', 'session-end', 'session-end-unanswered'])
export const endsByNativeExit = (phase: string): boolean => nativeExitPhases.has(phase)

const sleep = (millis: number) => new Promise<void>(resolve => setTimeout(resolve, millis))

function record(phase: Phase, cases: string[], exit?: number, error?: unknown): Promise<unknown> {
    const report = { ...phase, cases, success: error === undefined, ...(error === undefined ? {} : { error: String(error), failure: failureRecord(error) }) }
    return invoke('boundary_record', { report, exit: exit ?? null })
}

export async function runNativeExitPhase(phase: Phase, store: PersistentDataStore, fixture: Database): Promise<void> {
    const cases: string[] = []
    try {
        if (phase.phase === 'close-escape') await closeEscape(phase, cases)
        else await sessionEnd(phase, store, fixture, cases)
    } catch (error) {
        await record(phase, cases, 1, error)
    }
}

/** A document that never acknowledges a close: a close repeated after the limit closes the window natively. */
async function closeEscape(phase: Phase, cases: string[]): Promise<void> {
    let requests = 0
    await getCurrentWindow().onCloseRequested((event) => {
        event.preventDefault()
        requests += 1
    })
    const close = async (expected: number) => {
        await invoke('boundary_close_main')
        const limit = Date.now() + 5_000
        while (requests < expected) {
            if (Date.now() > limit) throw new Error(`close request ${expected} did not reach the document`)
            await sleep(20)
        }
    }
    await close(1)
    cases.push('unanswered-close-kept')
    await close(2)
    cases.push('early-repeat-kept')
    await record(phase, cases)
    // The native limit is five seconds from the first unanswered close.
    await sleep(5_500)
    await invoke('boundary_close_main')
    await sleep(10_000)
    throw new Error('a late repeated close left the window open')
}

/**
 * SIGTERM asks for a local flush with a token. The acknowledging document's answer exits the app,
 * so a commit scheduled a second after the answer must never land; an unanswered request still
 * exits at the native deadline.
 */
async function sessionEnd(phase: Phase, store: PersistentDataStore, fixture: Database, cases: string[]): Promise<void> {
    const acknowledges = phase.phase === 'session-end'
    if ((await store.replaceFromDatabase(fixture, undefined, [], [])).revision !== 1) throw new Error('fresh store')
    cases.push('commit')
    window.addEventListener('risu-native-lifecycle', (event) => {
        void (async () => {
            const detail = (event as CustomEvent<{ reason?: unknown; ackToken?: unknown }>).detail
            if (detail?.reason !== 'stop' || typeof detail.ackToken !== 'string' || !detail.ackToken) {
                throw new Error('session end did not request a stop flush with a token')
            }
            cases.push('stop-requested')
            if (!acknowledges) {
                await record(phase, cases)
                return
            }
            await store.replaceFromDatabase({ ...fixture, username: 'session-end-saved' } as Database, 1)
            cases.push('saved')
            await record(phase, cases)
            const bridge = (window as { RisuLifecycleBridge?: { onFlushComplete?: (token: string) => void } }).RisuLifecycleBridge
            if (typeof bridge?.onFlushComplete !== 'function') throw new Error('missing native flush bridge')
            bridge.onFlushComplete(detail.ackToken)
            await sleep(1_000)
            await store.replaceFromDatabase({ ...fixture, username: 'outlived-acknowledgement' } as Database, 2)
        })().catch(error => record(phase, cases, 1, error))
    })
    await invoke('boundary_ready')
}

export async function verifySessionEndReadback(store: PersistentDataStore): Promise<void> {
    const root = await store.readRoot()
    if (root.revision !== 2 || root.value.username !== 'session-end-saved') {
        throw new Error(`session end readback: revision ${root.revision}`)
    }
}
