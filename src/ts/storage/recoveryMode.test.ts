// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('../platform', () => ({ isTauri: true }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))

import {
    bootModeFor,
    clearBootTrail,
    markBootStage,
    markBootSuspect,
    readBootTrail,
    type BootAttemptBridge,
    type BootDecision,
} from './bootAttempt'
import {
    confirmRecoveryExclusions,
    decideBoot,
    finishBoot,
    isStartupExcluded,
    recoveryExclusions,
    recoveryState,
    startNormally,
    suspectedExclusions,
    toggleExclusion,
    type RecoveryExclusion,
} from './recoveryMode.svelte'

function bridge(decision: BootDecision): BootAttemptBridge & {
    begin: ReturnType<typeof vi.fn>
    complete: ReturnType<typeof vi.fn>
} {
    return {
        begin: vi.fn().mockResolvedValue(decision),
        complete: vi.fn().mockResolvedValue(undefined),
    }
}

describe('bootModeFor', () => {
    it('starts normally when the last start finished', () => {
        expect(bootModeFor(null)).toBe('normal')
        expect(bootModeFor({ consecutiveFailures: 0 })).toBe('normal')
    })

    it('offers the choice once, then opens the shell without asking', () => {
        expect(bootModeFor({ consecutiveFailures: 1 })).toBe('choose')
        expect(bootModeFor({ consecutiveFailures: 2 })).toBe('recovery')
        expect(bootModeFor({ consecutiveFailures: 9 })).toBe('recovery')
    })
})

describe('the boot trail', () => {
    beforeEach(() => {
        localStorage.clear()
        clearBootTrail()
    })

    it('keeps the stage and the suspect until the next start reads them', () => {
        markBootStage('plugins')
        markBootSuspect('plugin:translator')
        expect(readBootTrail()).toEqual({
            stage: 'plugins',
            suspect: 'plugin:translator',
        })
        clearBootTrail()
        expect(readBootTrail()).toEqual({ stage: null, suspect: null })
    })
})

describe('suspectedExclusions', () => {
    it('switches off what the trail was loading when the start stopped', () => {
        expect(
            suspectedExclusions({ stage: 'plugins', suspect: 'plugin:x' }),
        ).toEqual(['plugins'])
        expect(
            suspectedExclusions({ stage: 'account-bootstrap', suspect: null }),
        ).toEqual(['account'])
        expect(suspectedExclusions({ stage: null, suspect: null })).toEqual([])
    })
})

describe('the recovery shell', () => {
    beforeEach(async () => {
        localStorage.clear()
        clearBootTrail()
        // Return to an ordinary start between cases; the module keeps one state for the run.
        await decideBoot(bridge({ consecutiveFailures: 0 }))
    })

    it('starts normally and excludes nothing when the last start finished', async () => {
        const attempt = bridge({ consecutiveFailures: 0 })
        expect(await decideBoot(attempt)).toBe('normal')
        expect(attempt.begin).toHaveBeenCalledOnce()
        expect(recoveryExclusions()).toEqual([])
        expect(isStartupExcluded('plugins')).toBe(false)
    })

    it('opens the shell after a second failed start and preselects the suspect', async () => {
        markBootStage('plugins')
        markBootSuspect('plugin:translator')
        expect(
            await decideBoot(
                bridge({
                    consecutiveFailures: 2,
                    previous: {
                        startedAt: 10,
                        appVersion: '1.2.3',
                        consecutiveFailures: 1,
                    },
                }),
            ),
        ).toBe('recovery')
        const state = recoveryState()
        expect(state.excluded).toEqual(['plugins'])
        expect(state.trail.suspect).toBe('plugin:translator')
        expect(state.decision?.previous?.appVersion).toBe('1.2.3')
        // The trail is read once, so the next start judges itself.
        expect(readBootTrail()).toEqual({ stage: null, suspect: null })
    })

    it('applies the exclusions only once the shell hands over', async () => {
        await decideBoot(bridge({ consecutiveFailures: 1 }))
        toggleExclusion('sync')
        expect(recoveryExclusions()).toEqual([])
        expect(startNormally()).toEqual(['sync'])
        expect(isStartupExcluded('sync')).toBe(true)
        expect(isStartupExcluded('plugins')).toBe(false)
    })

    it('honours what the reader kept switched off on this device', async () => {
        await decideBoot(bridge({ consecutiveFailures: 0 }))
        expect(isStartupExcluded('plugins', ['plugins'])).toBe(true)
    })

    it('opens normally when the marker cannot be reached', async () => {
        const failing: BootAttemptBridge = {
            begin: vi.fn().mockRejectedValue(new Error('no marker')),
            complete: vi.fn(),
        }
        expect(await decideBoot(failing)).toBe('normal')
    })

    it('records that the start finished', async () => {
        const attempt = bridge({ consecutiveFailures: 0 })
        markBootStage('plugins')
        await finishBoot(attempt)
        expect(attempt.complete).toHaveBeenCalledOnce()
        expect(readBootTrail().stage).toBeNull()
    })
})

describe('confirmRecoveryExclusions', () => {
    const describeExclusion = (exclusion: RecoveryExclusion): string => exclusion

    it('writes nothing until the reader says so', async () => {
        const persist = vi.fn()
        expect(
            await confirmRecoveryExclusions(
                ['plugins'],
                async () => false,
                persist,
                describeExclusion,
                'keep {0} off?',
            ),
        ).toBe(false)
        expect(persist).not.toHaveBeenCalled()
    })

    it('keeps what the reader confirmed, naming it in the question', async () => {
        const persist = vi.fn()
        const ask = vi.fn().mockResolvedValue(true)
        expect(
            await confirmRecoveryExclusions(
                ['plugins', 'sync'],
                ask,
                persist,
                describeExclusion,
                'keep {0} off?',
            ),
        ).toBe(true)
        expect(ask).toHaveBeenCalledWith('keep plugins, sync off?')
        expect(persist).toHaveBeenCalledWith(['plugins', 'sync'])
    })

    it('asks nothing when the start excluded nothing', async () => {
        const ask = vi.fn()
        expect(
            await confirmRecoveryExclusions(
                [],
                ask,
                vi.fn(),
                describeExclusion,
                'keep {0} off?',
            ),
        ).toBe(false)
        expect(ask).not.toHaveBeenCalled()
    })
})
