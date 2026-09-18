import { beforeEach, describe, expect, it, vi } from 'vitest'

// Version-gate unit tests isolate admission; actual write fences have runtime coverage.
vi.mock('../storage/persistentDataRuntime.svelte', async (original) => ({
    ...(await original<object>()),
    assertPersistentMutationAllowed: vi.fn(),
    getPersistentStorageAuthorityEpoch: vi.fn(() => 0),
}))

vi.mock('../storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: () => undefined,
}))
vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))
vi.mock('./apiV3/v3.svelte', () => ({
    loadV3Plugins: vi.fn(async () => undefined),
}))

// The store module has to finish evaluating before the plugin graph pulls it in
// through the database module, otherwise its startup effect runs mid-cycle.
import '../stores.svelte'
import * as alertModule from '../alert'
import { loadV3Plugins } from './apiV3/v3.svelte'
import {
    getDatabase,
    setDatabaseLite,
    type Database,
} from '../storage/database.svelte'
import { importPlugin, loadPlugins, type RisuPlugin } from './plugins.svelte'

let alertError: ReturnType<typeof vi.spyOn>

function makePlugin(name: string, version: RisuPlugin['version']): RisuPlugin {
    return {
        name,
        version,
        enabled: true,
        script: '',
        realArg: {},
        arguments: {},
        customLink: [],
        argMeta: {},
    }
}

beforeEach(() => {
    vi.restoreAllMocks()
    vi.mocked(loadV3Plugins).mockClear()
    alertError = vi.spyOn(alertModule, 'alertError').mockImplementation(() => {})
    setDatabaseLite({
        characters: [],
        botPresets: [],
        botPresetsId: 0,
        plugins: [],
    } as unknown as Database)
})

describe('plugin API version gate', () => {
    it('loads only API 3.0 records and reports the stored older ones', async () => {
        const db = getDatabase()
        db.plugins = [makePlugin('legacy', '2.1'), makePlugin('modern', '3.0')]
        setDatabaseLite(db)

        await loadPlugins()

        expect(vi.mocked(loadV3Plugins)).toHaveBeenCalledTimes(1)
        expect(
            vi.mocked(loadV3Plugins).mock.calls[0][0].map((plugin) => plugin.name),
        ).toEqual(['modern'])
        expect(alertError).toHaveBeenCalledTimes(1)
        expect(alertError.mock.calls[0][0]).toContain('legacy')
        // The unsupported record is neither disabled nor removed.
        expect(
            getDatabase().plugins.map((plugin) => [plugin.name, plugin.enabled]),
        ).toEqual([
            ['legacy', true],
            ['modern', true],
        ])
    })

    it('loads without reporting when every enabled record is API 3.0', async () => {
        const db = getDatabase()
        db.plugins = [makePlugin('modern', '3.0')]
        setDatabaseLite(db)

        await loadPlugins()

        expect(alertError).not.toHaveBeenCalled()
        expect(
            vi.mocked(loadV3Plugins).mock.calls[0][0].map((plugin) => plugin.name),
        ).toEqual(['modern'])
    })

    it('refuses to install a bundle that declares API 2.1', async () => {
        await importPlugin('//@name legacy-bundle\n//@api 2.1\n\nconsole.log("x")')

        expect(alertError).toHaveBeenCalledTimes(1)
        expect(alertError.mock.calls[0][0]).toContain('2.1')
        expect(getDatabase().plugins).toHaveLength(0)
        expect(vi.mocked(loadV3Plugins)).not.toHaveBeenCalled()
    })

    it('refuses to install a bundle without an API banner', async () => {
        await importPlugin('//@name bannerless\n\nconsole.log("x")')

        expect(alertError).toHaveBeenCalledTimes(1)
        expect(getDatabase().plugins).toHaveLength(0)
    })

    it('installs a bundle that declares API 3.0', async () => {
        await importPlugin('//@name modern-bundle\n//@api 3.0\n\nconsole.log("x")')

        expect(alertError).not.toHaveBeenCalled()
        expect(getDatabase().plugins.map((plugin) => plugin.name)).toEqual([
            'modern-bundle',
        ])
        expect(getDatabase().plugins[0].version).toBe('3.0')
    })
})
