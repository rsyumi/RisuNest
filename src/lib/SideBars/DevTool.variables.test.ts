// @vitest-environment happy-dom

import { writable } from 'svelte/store'
import { afterEach, describe, expect, test, vi } from 'vitest'
import { flushSync, mount, tick, unmount } from 'svelte'
import { ActiveConversationSession, type ActiveConversationMutationEvent } from 'src/ts/storage/activeConversationSession'

const live = vi.hoisted(() => ({
    db: {} as Record<string, any>,
    session: null as unknown,
}))

vi.mock('src/ts/stores.svelte', () => ({
    DBState: { get db() { return live.db } },
    selectedCharID: writable(0),
    ReloadChatPointer: writable({}),
    ScriptDisplayRebuildPointer: writable({}),
    ScriptDisplayRefreshPointer: writable(0),
}))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => live.db }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireCompleteConversation: async () => ({ release: () => undefined }) as any,
    captureSelectedConversationTarget: () => ({ characterId: 'char-a', conversationId: 'chat-a' }) as any,
    getActiveConversationSession: () => live.session as any,
    peekActiveConversationSession: () => live.session as any,
    subscribeActiveConversationViewportSource: () => () => undefined,
}) satisfies Partial<typeof import('src/ts/storage/persistentDataRuntime.svelte')>)
vi.mock('@lucide/svelte', () => ({ HardDriveUploadIcon: () => {}, PlusIcon: () => {}, TrashIcon: () => {} }))
vi.mock('../Others/Help.svelte', () => ({ default: () => {} }))
vi.mock('../UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))
vi.mock('src/ts/globalApi.svelte', () => ({ getRequestLog: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertMd: vi.fn(), alertWait: vi.fn() }))
vi.mock('src/ts/tokenizer', () => ({ getCharToken: vi.fn(), getChatToken: vi.fn() }))
vi.mock('src/ts/process/prompt', () => ({ tokenizePreset: vi.fn() }))
vi.mock('src/ts/util', () => ({ selectSingleFile: vi.fn() }))
vi.mock('src/ts/process/index.svelte', () => ({
    doingChat: writable(false), previewFormated: [], previewBody: '', sendChat: vi.fn(),
}))
vi.mock('src/ts/conversationMutations', () => ({ appendCurrentConversationMessage: vi.fn() }))
vi.mock('src/ts/process/templates/chatTemplate', () => ({ applyChatTemplate: vi.fn(), chatTemplates: {} }))
vi.mock('src/ts/process/lorebook.svelte', () => ({ loadLoreBookV3PromptFromCompatibilitySnapshot: vi.fn() }))
vi.mock('src/ts/process/modules', () => ({ getModules: vi.fn(() => []) }))

import DevTool from './DevTool.svelte'

let mounted: ReturnType<typeof mount> | undefined
let target: HTMLDivElement | undefined

afterEach(async () => {
    try {
        if (mounted) await unmount(mounted)
    } finally {
        mounted = undefined
        target?.remove()
        live.session = null
    }
})

function inputAfterLabel(label: string): HTMLInputElement {
    const span = [...target!.querySelectorAll('span')].find((candidate) => candidate.textContent === label)
    const input = span?.nextElementSibling
    expect(input, `input for ${label}`).toBeInstanceOf(HTMLInputElement)
    return input as HTMLInputElement
}

describe('DevTool variables', () => {
    test('commits edited chat variables through the active conversation session', async () => {
        const conversation = {
            id: 'chat-a', message: [], note: '', name: 'Chat 1', localLore: [],
            scriptstate: { $mood: 'calm', $score: 3 },
        }
        live.db = { characters: [{ chaId: 'char-a', chatPage: 0, chats: [conversation] }] }
        const session = new ActiveConversationSession({
            characterId: 'char-a', conversationId: 'chat-a', conversation: conversation as any, storeRevision: 1,
        })
        live.session = session
        const events: ActiveConversationMutationEvent[] = []
        session.subscribe((event) => { if (event) events.push(event) })

        target = document.createElement('div')
        document.body.append(target)
        mounted = mount(DevTool, { target })
        await vi.waitFor(() => expect(target!.textContent).toContain('Variables'))
        const toggle = [...target.querySelectorAll('button')].find((button) => button.textContent?.trim() === 'Variables')
        toggle!.click()
        flushSync()
        await tick()

        const mood = inputAfterLabel('$mood')
        mood.value = 'calm!'
        mood.dispatchEvent(new Event('input', { bubbles: true }))
        const score = inputAfterLabel('$score')
        score.value = '7'
        score.dispatchEvent(new Event('input', { bubbles: true }))
        score.dispatchEvent(new Event('change', { bubbles: true }))
        flushSync()

        expect(events.map((event) => event.variableUpdate?.keys)).toEqual([['mood'], ['score']])
        expect(session.version).toBe(2)
        expect(session.readChatVariable('mood')).toBe('calm!')
        expect(session.readChatVariable('score')).toBe('7')
    })
})
