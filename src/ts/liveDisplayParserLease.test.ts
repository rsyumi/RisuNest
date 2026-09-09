import { expect, test, vi } from 'vitest'
import {
    captureLiveDisplayParserInputs,
    createLiveDisplayParserLeaseAcquirer,
} from './liveDisplayParserLease'
import { getDatabase, getCurrentCharacter, getCurrentChat } from './storage/database.svelte'
import { getModuleRegexScripts, getModuleTriggers } from './process/modules'

vi.mock('./storage/database.svelte', () => ({
    getDatabase: vi.fn(),
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
}))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({ getPersistentDataRuntime: vi.fn() }))
vi.mock('./process/modules', () => ({ getModuleRegexScripts: vi.fn(), getModuleTriggers: vi.fn() }))
vi.mock('./plugins/plugins.svelte', () => ({ pluginV2: { editdisplay: new Set() } }))
vi.mock('./util', () => ({ findCharacterbyId: vi.fn(), getPersonaPrompt: vi.fn() }))

function fixture() {
    let selected = {
        characterId: 'owner',
        conversationId: 'chat',
        navigationGeneration: 1,
        storeRevision: 1,
    }
    const release = vi.fn()
    const acquireCompleteConversation = vi.fn(async () => ({ release, target: selected }))
    const runtime = {
        captureSelectedConversationTarget: () => selected,
        acquireCompleteConversation,
    }
    const acquire = createLiveDisplayParserLeaseAcquirer({
        runtime: () => runtime as never,
        classify: (source) => ({ source }),
    })
    return {
        acquire,
        release,
        runtime,
        navigate: () => {
            selected = { ...selected, conversationId: 'other', navigationGeneration: 2 }
        },
    }
}

test('keeps plain live display windowed without acquiring history', async () => {
    const f = fixture()
    await expect(
        f.acquire({
            source: '<b>plain</b>',
            character: null,
            signal: new AbortController().signal,
        }),
    ).resolves.toBeNull()
    expect(f.runtime.acquireCompleteConversation).not.toHaveBeenCalled()
})

test('admits complete history for a plain group without reading its metadata-only body', async () => {
    const f = fixture()
    const readBody = vi.fn(() => {
        throw new Error('metadata-only body read')
    })
    const chat = {
        id: 'chat',
        get message() {
            return readBody()
        },
    }
    const group = { type: 'group', chaId: 'owner', chatPage: 0, chats: [chat], customscript: [] }
    vi.mocked(getDatabase).mockReturnValue({ characters: [group] } as never)
    vi.mocked(getCurrentCharacter).mockReturnValue(group as never)
    vi.mocked(getCurrentChat).mockReturnValue(chat as never)
    vi.mocked(getModuleRegexScripts).mockReturnValue([])
    vi.mocked(getModuleTriggers).mockReturnValue([])
    const acquire = createLiveDisplayParserLeaseAcquirer({
        runtime: () => f.runtime as never,
        classify: captureLiveDisplayParserInputs,
    })
    const lease = await acquire({
        source: 'plain group background',
        character: group as never,
        signal: new AbortController().signal,
    })
    expect(f.runtime.acquireCompleteConversation).toHaveBeenCalledOnce()
    expect(readBody).not.toHaveBeenCalled()
    lease!.release()
    expect(f.release).toHaveBeenCalledOnce()
})

test('holds a history lease until explicitly released and releases once', async () => {
    const f = fixture()
    const lease = await f.acquire({
        source: '{{history}}',
        character: null,
        signal: new AbortController().signal,
    })
    expect(f.release).not.toHaveBeenCalled()
    lease!.release()
    lease!.release()
    expect(f.release).toHaveBeenCalledTimes(1)
})

test.each(['abort', 'navigate'])('releases a late history lease after %s', async (action) => {
    const f = fixture()
    let resolve!: () => void
    f.runtime.acquireCompleteConversation.mockImplementationOnce(
        () =>
            new Promise((done) => {
                resolve = () =>
                    done({
                        release: f.release,
                        target: f.runtime.captureSelectedConversationTarget(),
                    })
            }),
    )
    const controller = new AbortController()
    const pending = f.acquire({ source: '{{history}}', character: null, signal: controller.signal })
    if (action === 'abort') controller.abort()
    else f.navigate()
    resolve()
    await expect(pending).rejects.toBeInstanceOf(Error)
    expect(f.release).toHaveBeenCalledTimes(1)
})
