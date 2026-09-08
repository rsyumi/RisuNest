import { get, writable } from 'svelte/store'

export type NavigationActivity = {
    token: number
    kind: 'character' | 'conversation'
}

export const navigationActivity = writable<NavigationActivity | null>(null)

let nextToken = 0

export function beginNavigationActivity(kind: NavigationActivity['kind']): {
    isCurrent(): boolean
    finish(): void
} {
    const token = ++nextToken
    navigationActivity.set({ token, kind })

    return {
        isCurrent: () => get(navigationActivity)?.token === token,
        finish: () =>
            navigationActivity.update((value) =>
                value?.token === token ? null : value,
            ),
    }
}
