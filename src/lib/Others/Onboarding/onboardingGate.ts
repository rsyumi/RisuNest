import { writable } from 'svelte/store'

/**
 * Keeps the onboarding on screen after a data path has replaced the working
 * set. The restored database carries its own `didFirstSetup`, so without the
 * hold the app would swap the onboarding out before its last screen.
 */
export const onboardingHold = writable(false)
