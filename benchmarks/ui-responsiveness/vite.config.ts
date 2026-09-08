import { execFileSync } from 'node:child_process'
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

const BASELINE_REVISION = '793930f17731547c7e9f2fe7bf0be6f6a5bbd462'
const repositoryRoot = path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    '..',
    '..',
)
const sourceMode =
    process.env.RISUNEST_UI_BENCH_SOURCE === 'baseline'
        ? 'baseline'
        : 'candidate'
const outputDirectory =
    process.env.RISUNEST_UI_BENCH_OUT_DIR ??
    path.join(
        repositoryRoot,
        'benchmarks',
        'ui-responsiveness',
        `.dist-${sourceMode}`,
    )

function git(argumentsList: string[]): string {
    return execFileSync(
        'git',
        [
            '-c',
            `safe.directory=${repositoryRoot.replaceAll('\\', '/')}`,
            ...argumentsList,
        ],
        {
            cwd: repositoryRoot,
            encoding: 'utf8',
            windowsHide: true,
            stdio: ['ignore', 'pipe', 'pipe'],
        },
    )
}

function baselineSourcePlugin(): Plugin | null {
    if (sourceMode !== 'baseline') return null
    const changedSourcePaths = new Set(
        git(['diff', '--name-only', BASELINE_REVISION, '--', 'src'])
            .split(/\r?\n/)
            .map((value) => value.trim().replaceAll('\\', '/'))
            .filter(Boolean),
    )
    const cache = new Map<string, string>()
    return {
        name: 'risunest-ui-benchmark-baseline-source',
        enforce: 'pre',
        load(id) {
            const cleanId = id.split('?', 1)[0]
            const relativePath = path
                .relative(repositoryRoot, cleanId)
                .replaceAll('\\', '/')
            if (!changedSourcePaths.has(relativePath)) return null
            let source = cache.get(relativePath)
            if (source === undefined) {
                source = git(['show', `${BASELINE_REVISION}:${relativePath}`])
                cache.set(relativePath, source)
            }
            return source
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
        plugins: [baselineSourcePlugin()],
        build: {
            outDir: outputDirectory,
            emptyOutDir: true,
            sourcemap: false,
            rolldownOptions: {
                input: path.join(
                    repositoryRoot,
                    'benchmarks',
                    'ui-responsiveness',
                    'index.html',
                ),
            },
        },
    }),
)
