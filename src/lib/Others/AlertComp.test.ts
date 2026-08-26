// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { alertStore } from 'src/ts/stores.svelte'

const branchMocks = vi.hoisted(() => ({
    getChatBranches: vi.fn(),
}))

vi.mock('src/ts/gui/branches', () => branchMocks)
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { characters: [], botPresets: [], botPresetsId: 0 } },
        alertStore: writable({ type: 'none', msg: '' }),
        selectedCharID: writable(-1),
    }
})
vi.mock('../../ts/alert', async () => {
    const { writable } = await import('svelte/store')
    return { alertGenerationInfoStore: writable(null) }
})
vi.mock('../../ts/characters', () => ({ getCharImage: vi.fn() }))
vi.mock('../../ts/parser/parser.svelte', () => ({ ParseMarkdown: vi.fn() }))
vi.mock('src/ts/characterCards', () => ({
    hubURL: '',
    isCharacterHasAssets: () => false,
}))
vi.mock('src/ts/globalApi.svelte', () => ({
    aiLawApplies: () => false,
    getFetchData: vi.fn(),
    getFetchLogs: () => [],
    openURL: vi.fn(),
}))
vi.mock('src/ts/tokenizer', () => ({
    tokenize: vi.fn(async (text: string) => `tokens:${text}`),
}))
vi.mock('src/ts/gui/colorscheme', async () => {
    const { writable } = await import('svelte/store')
    return { ColorSchemeTypeStore: writable(false) }
})
vi.mock('../../ts/sourcemap', () => ({ translateStackTrace: vi.fn() }))
vi.mock('src/ts/platform', () => ({
    isTauri: false,
    getDetailedOSLabel: async () => 'Test OS',
    getFallbackOSLabel: () => 'Test OS',
    getRisuEnvironmentLabel: () => 'Test',
}))
vi.mock('src/ts/storage/officialAccountMessage', () => ({
    isExpectedHubMessage: () => false,
}))
vi.mock('src/lang', () => ({
    language: new Proxy({}, { get: (_target, key) => String(key) }),
}))
vi.mock('@lucide/svelte', async () => {
    const { default: Stub } = await import('./AlertCompDependencyStub.test.svelte')
    return {
        CheckIcon: Stub,
        ChevronDownIcon: Stub,
        ChevronRightIcon: Stub,
        ChevronUpIcon: Stub,
        CopyIcon: Stub,
        User: Stub,
        XIcon: Stub,
    }
})
vi.mock('../SideBars/BarIcon.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('../UI/GUI/TextInput.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('../UI/GUI/Button.svelte', async () => ({
    default: (await import('./AlertCompButtonStub.test.svelte')).default,
}))
vi.mock('../UI/GUI/SelectInput.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('../UI/GUI/OptionInput.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('../UI/DeferredMarkdown.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('../UI/GUI/TextAreaInput.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('../Setting/Pages/Module/ModuleChatMenu.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))
vi.mock('./Help.svelte', async () => ({
    default: (await import('./AlertCompDependencyStub.test.svelte')).default,
}))

import { alertGenerationInfoStore } from '../../ts/alert'
import AlertComp from './AlertComp.svelte'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((next) => {
        resolve = next
    })
    return { promise, resolve }
}

function branch(preview: string, x = 0) {
    return {
        x,
        y: 1,
        connectX: -1,
        connectY: -1,
        content: preview,
        preview,
        multiChild: false,
        chatId: 0,
    }
}

describe('AlertComp branch view', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        alertStore.set({ type: 'none', msg: '' })
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    it('ignores a late authoritative branch result after the alert target changes', async () => {
        const firstResult = deferred<ReturnType<typeof branch>[]>()
        branchMocks.getChatBranches.mockImplementation((characterId: string) =>
            characterId === 'char-a'
                ? firstResult.promise
                : Promise.resolve([branch('current B')]),
        )
        alertStore.set({ type: 'branches', msg: 'char-a' })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(AlertComp, { target })
        await vi.waitFor(() => {
            expect(branchMocks.getChatBranches).toHaveBeenCalledWith('char-a')
        })

        alertStore.set({ type: 'branches', msg: 'char-b' })
        await vi.waitFor(() => expect(target.querySelectorAll('[role="table"]')).toHaveLength(1))
        firstResult.resolve([branch('stale A'), branch('stale A second', 1)])
        await firstResult.promise
        await tick()

        const renderedBranches = target.querySelectorAll<HTMLElement>('[role="table"]')
        expect(renderedBranches).toHaveLength(1)
        renderedBranches[0].dispatchEvent(new MouseEvent('mouseenter', { bubbles: true }))
        await tick()
        expect(target.textContent).toContain('current B')
        expect(target.textContent).not.toContain('stale A')
    })

    it('invalidates a late branch result when the alert closes before reopening the same target', async () => {
        const firstResult = deferred<ReturnType<typeof branch>[]>()
        const reopenedResult = deferred<ReturnType<typeof branch>[]>()
        branchMocks.getChatBranches
            .mockReturnValueOnce(firstResult.promise)
            .mockReturnValueOnce(reopenedResult.promise)
        alertStore.set({ type: 'branches', msg: 'char-a' })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(AlertComp, { target })
        await vi.waitFor(() => expect(branchMocks.getChatBranches).toHaveBeenCalledTimes(1))

        alertStore.set({ type: 'none', msg: '' })
        await tick()
        alertStore.set({ type: 'branches', msg: 'char-a' })
        await vi.waitFor(() => expect(branchMocks.getChatBranches).toHaveBeenCalledTimes(2))
        firstResult.resolve([branch('stale closed result')])
        await firstResult.promise
        await tick()

        expect(target.querySelectorAll('[role="table"]')).toHaveLength(0)
        reopenedResult.resolve([branch('fresh reopened result')])
        await vi.waitFor(() => expect(target.querySelectorAll('[role="table"]')).toHaveLength(1))
    })

    it('keeps generation details available after the live conversation is released', async () => {
        alertGenerationInfoStore.set({
            idx: 42,
            genInfo: {
                model: 'test-model',
                generationId: 'generation-a',
                inputTokens: 3,
                outputTokens: 5,
                maxContext: 100,
            },
            message: {
                role: 'char',
                data: 'detached generation message',
                chatId: 'message-a',
                saying: 'speaker-a',
                time: 123,
            },
        })
        alertStore.set({ type: 'requestdata', msg: 'generation-a' })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(AlertComp, { target })

        const buttons = target.querySelectorAll('button')
        expect(buttons.length).toBeGreaterThanOrEqual(4)
        buttons[1].click()
        await tick()

        expect(target.textContent).toContain('message-a')
        expect(target.textContent).toContain('speaker-a')
        expect(target.textContent).toContain('detached generation message')
    })
})
