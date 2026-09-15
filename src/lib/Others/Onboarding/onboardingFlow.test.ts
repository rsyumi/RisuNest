import { describe, expect, it } from 'vitest'

import {
    INITIAL_ONBOARDING_FLOW,
    ONBOARDING_STATES,
    accountRestoreApplied,
    goToOnboardingState,
    onboardingBack,
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
        expect(onboardingStep('sync-hub')).toBe(2)
        expect(onboardingStep('done')).toBe(3)
    })

    it('sends the reader back one screen at a time', () => {
        expect(onboardingBack('import')).toBe('home')
        expect(onboardingBack('sync')).toBe('home')
        expect(onboardingBack('sync-hub')).toBe('sync')
        expect(onboardingBack('sync-hub')).toBe('sync')
        expect(onboardingBack('sync-account')).toBe('sync')
        expect(onboardingBack('sync-account-found')).toBe('sync')
        expect(onboardingBack('sync-external')).toBe('sync')
    })

    it('offers no back link on the first and last screens', () => {
        expect(onboardingBack('home')).toBeNull()
        expect(onboardingBack('done')).toBeNull()
    })

    it('records the path each screen commits to', () => {
        const flow = INITIAL_ONBOARDING_FLOW
        expect(goToOnboardingState(flow, 'import').path).toBe('import')
        expect(goToOnboardingState(flow, 'sync-hub').path).toBe('hub')
        expect(goToOnboardingState(flow, 'sync-hub').path).toBe('hub')
        expect(goToOnboardingState(flow, 'sync-account').path).toBe('account')
        expect(goToOnboardingState(flow, 'sync-external').path).toBe('external')
    })

    it('keeps the current path on screens that choose none', () => {
        const flow: OnboardingFlow = { state: 'sync-hub', path: 'hub' }
        expect(goToOnboardingState(flow, 'done').path).toBe('hub')
    })

    it('resets the path to the first screen default when going home', () => {
        const flow: OnboardingFlow = { state: 'import', path: 'import' }
        expect(goToOnboardingState(flow, 'home')).toEqual({ state: 'home', path: 'fresh' })
    })

    it('lets a caller name the path itself', () => {
        const flow = INITIAL_ONBOARDING_FLOW
        expect(goToOnboardingState(flow, 'done', 'import')).toEqual({ state: 'done', path: 'import' })
    })


    it('names the closing sentence after the path that brought the data', () => {
        expect(onboardingSummary('fresh')).toBe('fresh')
        expect(onboardingSummary('import')).toBe('import')
        for (const path of ['hub', 'account', 'external'] as const) {
            expect(onboardingSummary(path)).toBe('data')
        }
    })

    it('advances account restore only after a snapshot was activated', () => {
        expect(accountRestoreApplied('activated')).toBe(true)
        expect(accountRestoreApplied('missing')).toBe(false)
        expect(accountRestoreApplied('unchanged')).toBe(false)
        expect(accountRestoreApplied('kept-local')).toBe(false)
    })
})
