import { describe, expect, it } from 'vitest'

import {
    INITIAL_ONBOARDING_FLOW,
    ONBOARDING_STATES,
    goToOnboardingState,
    onboardingBack,
    onboardingCloneNext,
    onboardingStep,
    onboardingSummary,
    type OnboardingFlow,
} from './onboardingFlow'

describe('onboarding flow', () => {
    it('starts on the first screen with no data path chosen', () => {
        expect(INITIAL_ONBOARDING_FLOW).toEqual({ state: 'home', path: 'fresh' })
    })

    it('numbers every screen so the step indicator always has a value', () => {
        for (const state of ONBOARDING_STATES) {
            expect([1, 2, 3]).toContain(onboardingStep(state))
        }
        expect(onboardingStep('home')).toBe(1)
        expect(onboardingStep('sync-device')).toBe(2)
        expect(onboardingStep('done')).toBe(3)
    })

    it('sends the reader back one screen at a time', () => {
        expect(onboardingBack('import')).toBe('home')
        expect(onboardingBack('sync')).toBe('home')
        expect(onboardingBack('sync-device')).toBe('sync')
        expect(onboardingBack('sync-hub')).toBe('sync')
        expect(onboardingBack('sync-account')).toBe('sync')
        expect(onboardingBack('sync-account-found')).toBe('sync')
    })

    it('offers no back link on the first and last screens', () => {
        expect(onboardingBack('home')).toBeNull()
        expect(onboardingBack('done')).toBeNull()
    })

    it('records the path each screen commits to', () => {
        const flow = INITIAL_ONBOARDING_FLOW
        expect(goToOnboardingState(flow, 'import').path).toBe('import')
        expect(goToOnboardingState(flow, 'sync-device').path).toBe('device')
        expect(goToOnboardingState(flow, 'sync-hub').path).toBe('hub')
        expect(goToOnboardingState(flow, 'sync-account').path).toBe('account')
    })

    it('keeps the current path on screens that choose none', () => {
        const flow: OnboardingFlow = { state: 'sync-device', path: 'device' }
        expect(goToOnboardingState(flow, 'done').path).toBe('device')
    })

    it('resets the path to the first screen default when going home', () => {
        const flow: OnboardingFlow = { state: 'import', path: 'import' }
        expect(goToOnboardingState(flow, 'home')).toEqual({ state: 'home', path: 'fresh' })
    })

    it('lets a caller name the path itself', () => {
        const flow = INITIAL_ONBOARDING_FLOW
        expect(goToOnboardingState(flow, 'done', 'import')).toEqual({ state: 'done', path: 'import' })
    })

    it('labels a download with a source that can actually send one', () => {
        for (const path of ['fresh', 'import', 'account'] as const) {
            const flow: OnboardingFlow = { state: 'sync', path }
            expect(goToOnboardingState(flow, 'sync-progress').path).toBe('device')
        }
        expect(goToOnboardingState({ state: 'sync-hub', path: 'hub' }, 'sync-progress').path).toBe('hub')
        expect(goToOnboardingState({ state: 'sync-device', path: 'device' }, 'sync-progress').path).toBe('device')
    })

    it('names the closing sentence after the path that brought the data', () => {
        expect(onboardingSummary('fresh')).toBe('fresh')
        expect(onboardingSummary('import')).toBe('import')
        expect(onboardingSummary('device')).toBe('device')
        for (const path of ['hub', 'account'] as const) {
            expect(onboardingSummary(path)).toBe('data')
        }
    })

    it('leaves the download screen only on a terminal clone phase', () => {
        for (const phase of ['idle', 'joined', 'confirmed', 'downloading', undefined] as const) {
            expect(onboardingCloneNext(phase)).toBeNull()
        }
        expect(onboardingCloneNext('completed')).toBe('done')
        expect(onboardingCloneNext('failed')).toBe('sync-device')
        expect(onboardingCloneNext('cancelled')).toBe('sync-device')
    })
})
