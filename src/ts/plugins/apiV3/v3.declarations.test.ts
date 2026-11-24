import { readFileSync } from 'node:fs'
import ts from 'typescript'
import { beforeAll, describe, expect, it, vi } from 'vitest'

const captured = vi.hoisted(() => ({ api: null as Record<string, any> | null, database: { characters: [], plugins: [] } as any }))

vi.mock('./factory', async () => (await import('./v3Host.testSupport')).sandboxHostModule((api) => { captured.api = api }))
vi.mock('../plugins.svelte', async () => {
    const host = await import('./v3Host.testSupport')
    return host.pluginsModule({ pluginStorageStore: host.pluginStorageStoreStub('declaration-plugin') })
})
vi.mock('src/ts/storage/database.svelte', async () => (await import('./v3Host.testSupport')).databaseModule({ read: () => captured.database, write: (database) => { captured.database = database } }))
vi.mock('../pluginSafeClass', async () => (await import('./v3Host.testSupport')).pluginSafeClassModule())
vi.mock('src/ts/stores.svelte', async () => (await import('./v3Host.testSupport')).storesModule({ read: () => captured.database, write: (database) => { captured.database = database } }))
vi.mock('src/ts/alert', async () => (await import('./v3Host.testSupport')).alertModule())
vi.mock('src/ts/util', async () => (await import('./v3Host.testSupport')).utilModule())
vi.mock('src/ts/globalApi.svelte', () => ({ checkCharOrder: vi.fn(), forageStorage: {}, getFetchLogs: vi.fn() }))
vi.mock('src/ts/gui/colorscheme', async () => (await import('./v3Host.testSupport')).colorschemeModule())
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/process/mcp/pluginmcp', async () => (await import('./v3Host.testSupport')).pluginMcpModule())
vi.mock('src/ts/process/files/inlays', async () => (await import('./v3Host.testSupport')).inlaysModule())
vi.mock('src/ts/translator/translator', async () => (await import('./v3Host.testSupport')).translatorModule())
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('localforage', async () => (await import('../localforage.testSupport')).localforageModule())
vi.mock('src/ts/process/index.svelte', async () => (await import('./v3Host.testSupport')).processIndexModule())
vi.mock('src/ts/model/modellist', async () => (await import('./v3Host.testSupport')).modellistModule())
vi.mock('src/ts/process/request/request', async () => (await import('./v3Host.testSupport')).requestModule())
vi.mock('src/ts/process/modules', async () => (await import('./v3Host.testSupport')).modulesModule())
vi.mock('src/ts/process/ttsHooks', async () => (await import('./v3Host.testSupport')).ttsHooksModule())
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => (await import('./v3Host.testSupport')).persistentRuntimeModule())

vi.mock('../pluginDatabaseAccess', async () => (await import('./v3Host.testSupport')).pluginDatabaseAccessModule())

import { executePluginV3 } from './v3.svelte'

/** Member names of each interface declared at the top level of the plugin declaration file. */
function declaredInterfaces(): Map<string, ts.InterfaceDeclaration> {
    const path = 'src/ts/plugins/apiV3/risuai.d.ts'
    const source = ts.createSourceFile(path, readFileSync(path, 'utf8'), ts.ScriptTarget.Latest, true)
    const interfaces = new Map<string, ts.InterfaceDeclaration>()
    source.forEachChild((node) => {
        if (ts.isInterfaceDeclaration(node)) interfaces.set(node.name.text, node)
    })
    return interfaces
}

const memberNames = (declaration: ts.InterfaceDeclaration) =>
    declaration.members.flatMap((member) => member.name ? [member.name.getText()] : [])

// Host methods upstream RisuAI also leaves out of its declaration file.
const undeclaredUpstreamMethods = ['alert', 'alertConfirm', 'alertError', 'installPlugin', 'risuFetch', 'setChatPanel']
// Defined by the guest script itself rather than by the host.
const guestHelpers = ['unwarpSafeArray']

describe('plugin API declarations', () => {
    const interfaces = declaredInterfaces()
    const declared = memberNames(interfaces.get('RisuaiPluginAPI')!)
    let host: Record<string, any>
    let guestVisible: Set<string>

    beforeAll(async () => {
        await executePluginV3({ name: 'declaration-plugin', script: '' } as any)
        host = captured.api!
        const properties = host._getPropertiesForInitialization()
        guestVisible = new Set([...Object.keys(host), ...properties.list, ...Object.keys(host._getAliases()), ...guestHelpers])
    })

    it('declares only members a plugin can reach, and no private member', () => {
        expect(declared.filter((name) => !guestVisible.has(name))).toEqual([])
        expect(declared.filter((name) => name.startsWith('_') || name.startsWith('risunest'))).toEqual([])
    })

    it('declares every public host method', () => {
        const undeclared = Object.keys(host).filter((name) =>
            !name.startsWith('_') && !name.startsWith('risunest') && !declared.includes(name))
        expect(undeclared.sort()).toEqual(undeclaredUpstreamMethods)
    })

    it('exposes the upstream parser promise and selected-character error through the public host', async () => {
        await expect(host.parseRisuChat('{{char}}')).rejects.toThrow('No character selected')
    })

    it('exposes the private RisuNest methods to the guest', () => {
        expect(Object.keys(host).filter((name) => name.startsWith('risunest')).sort()).toEqual([
            'risunestCallHostTool',
            'risunestCheckLuaSyntax',
            'risunestCreateCharacter',
            'risunestCreatePreset',
            'risunestDeleteLoadout',
            'risunestDeletePreset',
            'risunestListHostTools',
            'risunestListLoadouts',
            'risunestListPresets',
            'risunestListSettingKeys',
            'risunestOnChatView',
            'risunestOnGenerationEnd',
            'risunestPatchConversation',
            'risunestReadConversationContext',
            'risunestReadGlobalVariables',
            'risunestReadPreset',
            'risunestReadSettings',
            'risunestRestoreCharacter',
            'risunestSelectPreset',
            'risunestSetFloatingDragRegion',
            'risunestShowFloatingWindow',
            'risunestTrashCharacter',
            'risunestUpdatePreset',
            'risunestWriteGlobalVariables',
            'risunestWriteLoadout',
            'risunestWriteSettings',
        ])
    })

    it('backs every declared storage member with a host method', () => {
        const aliases: Record<string, Record<string, string>> = host._getAliases()
        for (const [property, methods] of Object.entries(aliases)) {
            const member = interfaces.get('RisuaiPluginAPI')!.members
                .find((candidate) => candidate.name?.getText() === property) as ts.PropertySignature
            const declaredType = interfaces.get(member.type!.getText())!
            expect(memberNames(declaredType).filter((name) => !Object.hasOwn(methods, name)), property).toEqual([])
            for (const method of Object.values(methods)) expect(typeof host[method], `${property} -> ${method}`).toBe('function')
        }
    })
})
