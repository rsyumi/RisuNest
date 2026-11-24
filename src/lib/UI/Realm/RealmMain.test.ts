import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, unmount } from 'svelte'
import { language } from 'src/lang'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { MobileGUI: writable(false), RealmInitialOpenChar: writable(null) }
})
vi.mock('src/ts/characterCards', () => ({
    getRisuHub: vi.fn(async () => []),
    downloadRisuHub: vi.fn(),
    hubAdditionalHTML: '',
}) satisfies Partial<typeof import('src/ts/characterCards')>)
vi.mock('src/ts/alert', () => ({ alertInput: vi.fn() }) satisfies Partial<typeof import('src/ts/alert')>)
vi.mock('@lucide/svelte', () => ({ ArrowLeft: () => {}, ArrowRight: () => {}, MenuIcon: () => {}, SearchIcon: () => {}, XIcon: () => {} }))
vi.mock('./RealmHubIcon.svelte', async () => await import('src/lib/Others/AlertCompDependencyStub.test.svelte'))
vi.mock('./RealmPopUp.svelte', async () => await import('src/lib/Others/AlertCompDependencyStub.test.svelte'))
vi.mock('./RealmBlockedCreators.svelte', async () => await import('src/lib/Others/AlertCompDependencyStub.test.svelte'))

import RealmMainHarness from './RealmMainHarness.test.svelte'

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

it('opens its menu in the chat screen window layer', async () => {
    const target = document.createElement('div')
    const windowLayer = document.createElement('div')
    document.body.append(target, windowLayer)
    instance = mount(RealmMainHarness, { target, props: { windowLayer } })
    flushSync()

    // The menu button follows the search field and its search button.
    target.querySelector('input')!.parentElement!.querySelectorAll('button')[1].click()
    // The hub sits in the chat pane, so its dialogs mount in the chat screen's window layer and cover the sidebar.
    await vi.waitFor(() => expect(windowLayer.textContent).toContain(language.manageBlockedRealmCreators))
    expect(target.textContent).not.toContain(language.manageBlockedRealmCreators)
})
