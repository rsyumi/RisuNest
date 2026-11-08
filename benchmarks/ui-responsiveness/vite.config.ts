import { execFileSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import {
    defineConfig,
    mergeConfig,
    normalizePath,
    type ConfigEnv,
    type Plugin,
    type UserConfig,
} from 'vite'
import mainConfigDefinition from '../../vite.config'

const BASELINE_REVISION = 'd6d9ba9411c7ed2ed3b4829f4616a458321dc791'
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

function gitPathSet(argumentsList: string[]): Set<string> {
    return new Set(
        git(argumentsList)
            .split(/\r?\n/)
            .map((value) => value.trim().replaceAll('\\', '/'))
            .filter(Boolean),
    )
}

interface BaselineTree {
    changedSourcePaths: Set<string>
    baselineSourcePaths: Set<string>
    cache: Map<string, string>
}

let baselineTree: BaselineTree | undefined

// The page and every worker bundle share one listing of the baseline tree.
function readBaselineTree(): BaselineTree {
    baselineTree ??= {
        // With rename detection a renamed file is listed only under its new path.
        changedSourcePaths: gitPathSet([
            'diff',
            '--name-only',
            '--no-renames',
            BASELINE_REVISION,
            '--',
            'src',
        ]),
        baselineSourcePaths: gitPathSet([
            'ls-tree',
            '-r',
            '--name-only',
            BASELINE_REVISION,
            '--',
            'src',
        ]),
        cache: new Map(),
    }
    return baselineTree
}

function baselineSourcePlugin(): Plugin | null {
    if (sourceMode !== 'baseline') return null
    const { changedSourcePaths, baselineSourcePaths, cache } =
        readBaselineTree()
    let extensions: string[] = []
    return {
        name: 'risunest-ui-benchmark-baseline-source',
        enforce: 'pre',
        configResolved(config) {
            extensions = config.resolve.extensions
        },
        // The default resolver only sees the working tree, so a baseline import
        // of a file that was deleted or renamed since then does not resolve.
        // Resolve it against the baseline tree, trying candidates in the order
        // Vite does, and let `load` serve the baseline contents.
        resolveId: {
            filter: { id: /^(?:\.{1,2}\/|\/?src\/)/ },
            handler(source, importer) {
                const queryIndex = source.indexOf('?')
                const specifier =
                    queryIndex === -1 ? source : source.slice(0, queryIndex)
                const query = queryIndex === -1 ? '' : source.slice(queryIndex)
                let target: string
                if (specifier.startsWith('.')) {
                    if (!importer) return null
                    target = path.resolve(
                        path.dirname(importer.split('?', 1)[0]),
                        specifier,
                    )
                } else {
                    target = path.join(repositoryRoot, specifier)
                }
                const relativeTarget = path
                    .relative(repositoryRoot, target)
                    .replaceAll('\\', '/')
                const match = [
                    relativeTarget,
                    ...extensions.map(
                        (extension) => `${relativeTarget}${extension}`,
                    ),
                    ...extensions.map(
                        (extension) => `${relativeTarget}/index${extension}`,
                    ),
                ].find((candidate) => baselineSourcePaths.has(candidate))
                if (!match) return null
                const matchPath = path.join(repositoryRoot, match)
                if (existsSync(matchPath)) return null
                return `${normalizePath(matchPath)}${query}`
            },
        },
        load(id) {
            // An id with a query is a module derived from the file, such as a
            // Svelte component's styles, and its own plugin builds it.
            if (id.includes('?')) return null
            const relativePath = path
                .relative(repositoryRoot, id)
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
        // Workers are bundled through their own plugin list, which the page's
        // plugins do not reach.
        worker: { plugins: () => [baselineSourcePlugin()] },
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
