// @vitest-environment node

import { beforeAll, expect, test, vi } from 'vitest'

const runtimeState = vi.hoisted(() => ({
    createLuaFactory: vi.fn(),
}))

vi.mock('../parser/parser.svelte', () => ({ hasher: vi.fn(), risuChatParser: vi.fn() }))
vi.mock('../alert', () => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertInput: vi.fn(),
    alertNormal: vi.fn(),
    alertSelect: vi.fn(),
}))
vi.mock('../globalApi.svelte', () => ({ fetchNative: vi.fn(), readImage: vi.fn() }))
vi.mock('../platform', () => ({ isTauriMobile: true }))
vi.mock('../tokenizer', () => ({ tokenize: vi.fn() }))
vi.mock('../util', () => ({
    asBuffer: vi.fn(),
    getPersonaPrompt: vi.fn(),
    getUserIcon: vi.fn(),
    getUserName: vi.fn(),
}))
vi.mock('../storage/database.svelte', () => ({
    getCurrentCharacter: vi.fn(() => ({})),
    getCurrentChat: vi.fn(() => ({ message: [] })),
    getDatabase: vi.fn(() => ({ characters: [] })),
    setDatabase: vi.fn(),
}))
vi.mock('../stores.svelte', () => ({
    DBState: { db: {} },
    ReloadChatPointer: { update: vi.fn() },
    ReloadGUIPointer: { update: vi.fn() },
    selectedCharID: { subscribe: (run: (value: number) => void) => (run(0), () => undefined) },
}))
vi.mock('./modules', () => ({ getModuleLorebooks: vi.fn(() => []), getModuleTriggers: vi.fn(() => []) }))
vi.mock('./files/inlays', () => ({ getInlayAsset: vi.fn(), writeInlayImage: vi.fn() }))
vi.mock('./lorebook.svelte', () => ({ loadLoreBookV3PromptFromCompatibilitySnapshot: vi.fn() }))
vi.mock('./memory/hypamemory', () => ({ HypaProcesser: vi.fn() }))
vi.mock('./request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('./stableDiff', () => ({ generateAIImage: vi.fn() }))
vi.mock('./luaRuntime', () => ({
    createLuaFactory: () => runtimeState.createLuaFactory(),
    runLuaSource: vi.fn(async (engine, source) => engine.doString(source)),
}))

let runScripted: typeof import('./scriptings').runScripted

beforeAll(async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('', { status: 200 })))
    runScripted = (await import('./scriptings')).runScripted
})

test('rethrows a shared factory failure to concurrent callers and retries later', async () => {
    const failure = new Error('factory initialization failed')
    let rejectFactory!: (error: Error) => void
    const failedFactory = new Promise<never>((_resolve, reject) => {
        rejectFactory = reject
    })
    runtimeState.createLuaFactory.mockReturnValueOnce(failedFactory)

    const arg = {
        char: { chaId: 'factory-failure' } as never,
        chat: { message: [] } as never,
        mode: 'factory-failure',
    }
    const first = runScripted('', arg)
    const concurrent = runScripted('', arg)
    rejectFactory(failure)

    const failedResults = await Promise.allSettled([first, concurrent])
    expect(failedResults).toEqual([
        { status: 'rejected', reason: failure },
        { status: 'rejected', reason: failure },
    ])
    expect(runtimeState.createLuaFactory).toHaveBeenCalledTimes(1)

    runtimeState.createLuaFactory.mockResolvedValueOnce({
        mountFile: vi.fn(),
        createEngine: vi.fn(async () => ({
            global: { close: vi.fn(), get: vi.fn(), set: vi.fn() },
            doString: vi.fn(),
        })),
    })

    await expect(runScripted('', arg)).resolves.toEqual(
        expect.objectContaining({ stopSending: false }),
    )
    expect(runtimeState.createLuaFactory).toHaveBeenCalledTimes(2)
})
