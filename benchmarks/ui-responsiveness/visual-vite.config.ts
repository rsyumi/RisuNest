import path from 'node:path'
import { fileURLToPath } from 'node:url'

import {
    defineConfig,
    mergeConfig,
    type ConfigEnv,
    type Plugin,
    type UserConfig,
} from 'vite'
import mainConfigDefinition from '../../vite.config'

const repositoryRoot = path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    '..',
    '..',
)
const benchmarkDirectory = path.join(
    repositoryRoot,
    'benchmarks',
    'ui-responsiveness',
)
const outputDirectory =
    process.env.RISUNEST_UI_VISUAL_OUT_DIR ??
    path.join(benchmarkDirectory, '.visual-dist')

function chatScreenDependencyStubs(): Plugin {
    const emptyStub = path.join(benchmarkDirectory, 'VisualEmptyStub.svelte')
    const contentStub = path.join(
        benchmarkDirectory,
        'VisualChatContentStub.svelte',
    )
    const utilStub = path.join(benchmarkDirectory, 'visual-util-stub.ts')
    const emptyImports = new Set([
        './ResizeBox.svelte',
        './TransitionImage.svelte',
        './BackgroundDom.svelte',
        '../UI/GUI/SideBarArrow.svelte',
        '../Others/ChatList.svelte',
        '../Setting/Pages/Module/ModuleChatMenu.svelte',
    ])
    return {
        name: 'risunest-ui-visual-chat-screen-stubs',
        enforce: 'pre',
        async resolveId(source, importer, options) {
            if (
                !importer
                    ?.replaceAll('\\', '/')
                    .endsWith('/src/lib/ChatScreens/ChatScreen.svelte')
            )
                return null
            const replacement =
                source === '../../ts/util'
                    ? utilStub
                    : source === './DefaultChatScreen.svelte'
                      ? contentStub
                      : emptyImports.has(source)
                        ? emptyStub
                        : null
            if (!replacement) return null
            return this.resolve(replacement, importer, {
                ...options,
                skipSelf: true,
            })
        },
    }
}

async function resolveMainConfig(): Promise<UserConfig> {
    const environment: ConfigEnv = {
        command: 'build',
        mode: 'agent',
        isSsrBuild: false,
        isPreview: false,
    }
    return typeof mainConfigDefinition === 'function'
        ? await mainConfigDefinition(environment)
        : mainConfigDefinition
}

export default defineConfig(async () =>
    mergeConfig(await resolveMainConfig(), {
        root: repositoryRoot,
        publicDir: false,
        plugins: [chatScreenDependencyStubs()],
        build: {
            outDir: outputDirectory,
            emptyOutDir: true,
            sourcemap: false,
            rolldownOptions: {
                input: path.join(benchmarkDirectory, 'visual-index.html'),
            },
        },
    }),
)
