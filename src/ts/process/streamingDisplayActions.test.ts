import { afterEach, describe, expect, it, vi } from 'vitest'
import type { character, customscript } from '../storage/database.svelte'
import { createStreamingDisplayController } from './streamingDisplayScheduler'

const mocks = vi.hoisted(() => {
    const counts = { regex: 0, lua: 0, plugin: 0, preview: 0 }
    const emotionStore = {
        set() {
            counts.regex += 1
        },
    }
    return {
        counts,
        emotionStore,
        selectedCharStore: {},
        pluginAction: async (data: string) => {
            counts.plugin += 1
            return data
        },
        database: {
            dynamicAssets: false,
            presetRegex: [] as customscript[],
            characters: [] as never[],
        },
    }
})

vi.mock('svelte/store', () => ({
    get: (store: unknown) => store === mocks.emotionStore ? {} : 0,
}))
vi.mock('src/ts/stores.svelte', () => ({
    CharEmotion: mocks.emotionStore,
    selectedCharID: mocks.selectedCharStore,
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => mocks.database,
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    peekActiveConversationSession: () => null,
}))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('src/ts/util', () => ({ selectSingleFile: vi.fn() }))
vi.mock('src/ts/parser/parser.svelte', () => ({
    assetRegex: /$^/g,
    risuChatParser: (data: string) => data,
}))
vi.mock('src/ts/process/modules', () => ({
    getModuleAssets: () => [],
    getModuleRegexScripts: () => [],
}))
vi.mock('src/ts/process/memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('src/ts/process/scriptings', () => ({
    runLuaEditTrigger: async (_char: unknown, _mode: unknown, data: string) => {
        mocks.counts.lua += 1
        return data
    },
}))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginV2: {
        editinput: new Set(),
        editoutput: new Set([mocks.pluginAction]),
        editprocess: new Set(),
        editdisplay: new Set(),
    },
}))
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn() }))

const { processScriptFull, resetScriptCache } = await import('./scripts')

const actionCharacter = {
    type: 'character',
    chaId: 'stream-action-counts',
    customscript: [{
        comment: 'record each semantic invocation',
        in: '^',
        out: '@@emo happy',
        type: 'editoutput',
        flag: '',
        ableFlag: true,
    }],
    emotionImages: [['happy', 'happy.png']],
} as character

describe('streaming display action invocation counts', () => {
    afterEach(() => {
        vi.useRealTimers()
    })

    it.each([
        { mode: 'off' as const, semanticCount: 4, previewCount: 0 },
        { mode: 'balanced' as const, semanticCount: 2, previewCount: 0 },
        { mode: 'strong' as const, semanticCount: 1, previewCount: 2 },
    ])('runs regex, Lua, and plugin actions with $mode semantics', async ({ mode, semanticCount, previewCount }) => {
        vi.useFakeTimers()
        vi.setSystemTime(0)
        resetScriptCache()
        Object.assign(mocks.counts, { regex: 0, lua: 0, plugin: 0, preview: 0 })
        const controller = createStreamingDisplayController({
            mode,
            processSemantic: async ({ value }) => {
                await processScriptFull(
                    actionCharacter,
                    value,
                    'editoutput',
                    -1,
                    {},
                    { cache: 'bypass', regexWorker: false },
                )
            },
            processPreview: async () => {
                mocks.counts.preview += 1
            },
        })

        await controller.submit('a')
        await vi.advanceTimersByTimeAsync(10)
        await controller.submit('ab')
        await controller.submit('abc')
        await controller.submit('abcd')
        await controller.finish()

        expect(mocks.counts).toEqual({
            regex: semanticCount,
            lua: semanticCount,
            plugin: semanticCount,
            preview: previewCount,
        })
    })
})
