import { bench, expect, vi } from "vitest"
import { makeRegexFixture, fnv1a } from "./tests/phase1Fixtures"

const mocks = vi.hoisted(() => ({
    database: { dynamicAssets: false, presetRegex: [] as never[], characters: [] as never[] },
}))

vi.mock("svelte/store", () => ({
    get: vi.fn(),
    writable: vi.fn(() => ({ subscribe: vi.fn(), set: vi.fn(), update: vi.fn() })),
}))
vi.mock("src/ts/stores.svelte", () => ({ CharEmotion: {}, selectedCharID: {} }))
vi.mock("src/ts/storage/database.svelte", () => ({
    getDatabase: () => mocks.database,
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
}))
vi.mock("src/ts/globalApi.svelte", () => ({ downloadFile: vi.fn() }))
vi.mock("src/ts/alert", () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock("src/lang", () => ({ language: {} }))
vi.mock("src/ts/util", () => ({ selectSingleFile: vi.fn() }))
vi.mock("src/ts/parser/parser.svelte", () => ({
    assetRegex: /$^/g,
    risuChatParser: (data: string) => data,
}))
vi.mock("src/ts/process/modules", () => ({
    getModuleAssets: () => [],
    getModuleRegexScripts: () => [],
}))
vi.mock("src/ts/process/memory/hypamemory", () => ({ HypaProcesser: class {} }))
vi.mock("src/ts/process/scriptings", () => ({
    runLuaEditTrigger: async (_char: unknown, _mode: unknown, data: string) => data,
}))
vi.mock("src/ts/plugins/plugins.svelte", () => ({
    pluginV2: { editinput: new Set(), editoutput: new Set(), editprocess: new Set(), editdisplay: new Set() },
}))
vi.mock("src/ts/process/triggers", () => ({ runTrigger: vi.fn() }))

const { processScriptFull, resetScriptCache } = await import("./scripts")

for (const ruleCount of [20, 100, 500] as const) {
    const fixture = makeRegexFixture(ruleCount)
    const character = { type: "simple" as const, chaId: "phase1", customscript: fixture.scripts }

    bench(`${ruleCount} editoutput rules`, async () => {
        resetScriptCache()
        const result = await processScriptFull(character, fixture.input, "editoutput")
        expect(fnv1a(result.data)).toBe(fixture.expectedHash)
    })
}
