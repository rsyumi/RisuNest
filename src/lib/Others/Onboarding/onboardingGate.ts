import { get, writable } from 'svelte/store'

/**
 * Keeps the onboarding on screen after a data path has replaced the working
 * set. The restored database carries its own `didFirstSetup`, so without the
 * hold the app would swap the onboarding out before its last screen.
 */
export const onboardingHold = writable(false)

/**
 * Whether first setup is finished and the onboarding is gone. A restore during
 * onboarding sets `didFirstSetup` while the onboarding stays.
 */
export function onboardingFinished(db: { didFirstSetup?: boolean } | undefined): boolean {
    return !!db?.didFirstSetup && !get(onboardingHold)
}
