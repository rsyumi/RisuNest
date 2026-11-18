import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import { resolve } from 'node:path'
import { lazyAppBoundaries } from './lazyAppPlugin'
import { settledThoughtBoundaries } from './settledThoughtPlugin'
import { sidebarProductBoundaries } from './sidebarProductPlugin'
import tailwindcss from '@tailwindcss/vite'
import { autoStubs, stubbed } from './autoStubPlugin'

const adapters = new Set(["../plugins.svelte", "src/ts/storage/database.svelte", "src/ts/storage/workingSetCatalog", "../pluginSafeClass", "../pluginClaimSession", "src/ts/stores.svelte", "src/ts/util", "src/ts/alert", "src/lang", "src/ts/globalApi.svelte", "src/ts/gui/colorscheme", "src/ts/process/mcp/pluginmcp", "src/ts/process/files/inlays", "src/ts/translator/translator", "src/ts/parser/parser.svelte", "src/ts/storage/nativePluginPermissions", "src/ts/process/index.svelte", "src/ts/process/generationState", "src/ts/model/modellist", "src/ts/process/request/request", "src/ts/process/modules", "src/ts/process/ttsHooks", "src/ts/storage/persistentDataRuntime.svelte", "src/ts/conversationMutations", "../pluginDatabaseAccess"])
export default defineConfig(({ mode }) => {
    if (mode !== 'agent') throw new Error('Browser fixture requires agent mode')
    return {
        root: resolve(import.meta.dirname),
        define: { 'import.meta.env.VITE_RISU_LEGAL_CONFIGURED': 'true' },
        plugins: [tailwindcss(), autoStubs(), lazyAppBoundaries(), settledThoughtBoundaries(), sidebarProductBoundaries(), {
            name: 'fixture-host-boundaries', enforce: 'pre',
            async resolveId(requested, importer, options) {
                const source = requested.replaceAll('\\', '/').replace(resolve(import.meta.dirname, '../../src').replaceAll('\\', '/') + '/', 'src/')
                const owner = importer?.replaceAll('\\', '/') ?? ''
                const adapter = (file: string) => stubbed(this, resolve(import.meta.dirname, file).replaceAll('\\', '/'), requested, importer!, options)
                if (/\/src\/lib\/Others\/(?:MonacoEditor|TextEditorMonaco)\.svelte$/.test(owner) && ['src/ts/gui/codearea/cbsMonaco', 'src/ts/gui/colorscheme', 'src/lang'].includes(source)) {
                    return adapter('monacoAdapters.ts')
                }
                if (owner.endsWith('/src/ts/ui/modalNavigation.ts') && /(?:^|\/)stores\.svelte$/.test(source)) {
                    return adapter('modalNavigationAdapters.ts')
                }
                if (/\/src\/lib\/(SideBars\/(LoreBook\/|Scripts\/(Regex|TriggerV1))|Setting\/Pages\/PromptSettings|UI\/(PromptDataItem|GUI\/ListPager|Accordion))/.test(owner)) {
                    if (/(^|\/)(stores\.svelte|presetEditor\.svelte|util|alert|lang|database\.svelte|tokenizer|scripts|prompt|templateCheck)$/.test(source)) {
                        return adapter('dragDropAdapters.svelte.ts')
                    }
                    if (/(Help|TextAreaInput|ToggleDefinitionEditor|ModelList|AuxModelSelectors)\.svelte$/.test(source)) {
                        return resolve(import.meta.dirname, 'Empty.svelte').replaceAll('\\', '/')
                    }
                }
                if (owner.endsWith('/src/lib/Setting/Pages/PersonaSettings.svelte')) {
                    if (/(^|\/)(stores\.svelte|util|alert|lang|characters|persona|deletedOwnerAssetCleanup|persistentDataRuntime\.svelte)$/.test(source)) {
                        return adapter('dragDropAdapters.svelte.ts')
                    }
                    if (/TextAreaInput\.svelte$/.test(source)) {
                        return resolve(import.meta.dirname, 'Empty.svelte').replaceAll('\\', '/')
                    }
                }
                if (owner.endsWith('/src/lib/SideBars/SideChatList.svelte')) {
                    if (/(^|\/)(stores\.svelte|characters|alert|util|lang|chatBindings\.svelte|globalApi\.svelte|workingSetCatalog)$/.test(source)) {
                        return adapter('chatListAdapters.svelte.ts')
                    }
                    if (/(Toggles|ModuleScopeButton)\.svelte$/.test(source)) {
                        return resolve(import.meta.dirname, 'Empty.svelte').replaceAll('\\', '/')
                    }
                }
                if (owner.endsWith('/src/ts/plugins/apiV3/v3.svelte.ts') && adapters.has(source)) {
                    return adapter('hostAdapters.ts')
                }
                if (!/realmEndpoints(\.ts)?$/.test(source)) return null
                const resolved = await this.resolve(requested, importer, { ...options, skipSelf: true })
                if (!resolved?.id.replaceAll('\\', '/').endsWith('/src/ts/realmEndpoints.ts')) return null
                return resolved.id.replace(/realmEndpoints\.ts$/, 'realmEndpoints.blocked.ts')
            },
        }, svelte()],
        resolve: { alias: [
            { find: /^monaco-editor$/, replacement: resolve(import.meta.dirname, '../../src/ts/gui/codearea/monacoRuntime.ts') },
            { find: 'src', replacement: resolve(import.meta.dirname, '../../src') },
        ] },
        worker: { format: 'es' },
        server: { hmr: false, host: '127.0.0.1', port: 4187, strictPort: true, fs: { allow: [resolve(import.meta.dirname, '../..')] } },
        build: {
            outDir: resolve(import.meta.dirname, '../../.tmp/test-results/browser/dist'), emptyOutDir: true,
            rollupOptions: {
                preserveEntrySignatures: 'strict',
                input: [resolve(import.meta.dirname, 'imageFormats.html'), resolve(import.meta.dirname, 'index.html'), resolve(import.meta.dirname, 'dragDrop.html'), resolve(import.meta.dirname, 'chatList.html'), resolve(import.meta.dirname, 'sidebarTouch.html'), resolve(import.meta.dirname, 'modalNavigation.html'), resolve(import.meta.dirname, 'lazyApp.html'), resolve(import.meta.dirname, 'monaco.html'), resolve(import.meta.dirname, 'settledThought.html'), resolve(import.meta.dirname, 'switchContainment.html'), resolve(import.meta.dirname, 'chatRendering.html')],
            },
        },
    }
})
