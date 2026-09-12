import ts from 'typescript'
import { execFileSync } from 'node:child_process'
import path from 'node:path'

// Build-only timing. No runtime source file or user-data accessor is installed.
const observed = {
    '/src/ts/storage/databasePreparation.ts': {
        prepareDatabaseForPersistence: 'prepare-import',
        prepareDatabaseForBootstrap: 'prepare-bootstrap',
        prepareDetachedDatabase: 'normalize',
    },
    '/src/ts/storage/saveCoordinatorHelpers.ts': { canonicalJson: 'canonical' },
    '/src/ts/storage/saveCoordinator.ts': {
        initialize: 'baseline',
        capture: 'capture',
        captureDatabase: 'capture-database',
        flushIterations: 'flush',
    },
    '/src/ts/bootstrap.ts': { loadData: 'bootstrap', cleanChunks: 'clean-chunks' },
    '/src/ts/globalApi.svelte.ts': { saveDb: 'save-observer' },
}

export function instrumentSource(source, id) {
    const normalizedId = id.replaceAll('\\', '/').split('?')[0]
    const suffix = Object.keys(observed).find((key) => normalizedId.endsWith(key))
    if (!suffix) return null
    const names =
        suffix.endsWith('databasePreparation.ts') &&
        !source.includes('function prepareDatabaseForBootstrap(')
            ? { prepareDatabaseForPersistence: 'prepare-import' }
            : observed[suffix]
    const file = ts.createSourceFile(id, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS)
    const edits = []
    const found = new Set()
    const wrap = (body, label) => {
        const variable = '__startupObservation'
        edits.push({
            at: body.getStart(file) + 1,
            text: `
            const ${variable} = performance.now();
            window.__startupBegin?.('${label}');
            let __startupSuccess = true;
            try {
        `,
        })
        edits.push({
            at: body.end - 1,
            text: `
            } catch (__startupError) { __startupSuccess = false; throw __startupError; }
            finally { window.__startupRecord?.('${label}', ${variable}, __startupSuccess); }
        `,
        })
    }
    const visit = (node) => {
        if (suffix.endsWith('bootstrap.ts') && ts.isIfStatement(node)) {
            let owner = node.parent
            while (owner && !ts.isFunctionDeclaration(owner)) owner = owner.parent
            const condition = node.expression.getText(file)
            const reason =
                owner?.name?.getText(file) !== 'cleanChunks'
                    ? 0
                    : condition === 'hasIncompletePersistentWorkingSet(db, workingSetResidency)'
                      ? 1
                      : condition === 'db.account?.useSync'
                        ? 2
                        : condition === 'db.coldstorage && !cleanColdStorage'
                          ? 3
                          : 0
            if (reason) {
                const statement = node.thenStatement
                edits.push({
                    at: statement.getStart(file),
                    text: `{ window.__startupMetrics.cleanChunksReason = ${reason}; `,
                })
                edits.push({ at: statement.end, text: ' }' })
            }
        }
        if (
            (ts.isFunctionDeclaration(node) || ts.isMethodDeclaration(node)) &&
            node.body &&
            node.name
        ) {
            const name = node.name.getText(file)
            if (names[name]) {
                found.add(name)
                wrap(node.body, names[name])
            }
        }
        if (
            suffix.endsWith('globalApi.svelte.ts') &&
            ts.isCallExpression(node) &&
            node.expression.getText(file) === '$effect'
        ) {
            let ancestor = node.parent
            while (ancestor && !ts.isFunctionDeclaration(ancestor)) ancestor = ancestor.parent
            if (ancestor?.name?.getText(file) === 'saveDb') {
                const callback = node.arguments[0]
                if (callback && ts.isArrowFunction(callback) && ts.isBlock(callback.body)) {
                    wrap(
                        callback.body,
                        callback.body.getText(file).includes('estimateRootBytes')
                            ? 'save-effect-root'
                            : 'save-effect-character',
                    )
                }
            }
        }
        ts.forEachChild(node, visit)
    }
    visit(file)
    for (const name of Object.keys(names)) {
        if (!found.has(name)) throw new Error(`Missing benchmark instrumentation point: ${name}`)
    }
    for (const edit of edits.sort((a, b) => b.at - a.at)) {
        source = source.slice(0, edit.at) + edit.text + source.slice(edit.at)
    }
    return source
}

export function startupObservationPlugin() {
    return { name: 'synthetic-startup-observation', enforce: 'pre', transform: instrumentSource }
}

export function baselineFrontendPlugin() {
    const files = new Set([
        'src/App.svelte',
        'src/ts/bootstrap.ts',
        'src/ts/stores.svelte.ts',
        'src/ts/storage/databasePreparation.ts',
        'src/ts/storage/persistentBootstrap.ts',
        'src/lib/UI/GUI/LoadingIndicator.svelte',
        'src/lang/en.ts',
        'src/lang/ko.ts',
    ])
    return {
        name: 'synthetic-startup-gc-only-baseline',
        enforce: 'pre',
        load(id) {
            if (process.env.STARTUP_BENCHMARK_VARIANT !== 'gc-only' || id.includes('?')) return null
            const relative = path.relative(process.cwd(), id).replaceAll('\\', '/')
            if (!files.has(relative)) return null
            // Only the original frontend is read. Native open is always the real modified command.
            return execFileSync(
                'git',
                [
                    '-c',
                    `safe.directory=${process.cwd().replaceAll('\\', '/')}`,
                    'show',
                    `dab32f2b585bbdd3b7e6d8ae4f8d52060350858e:${relative}`,
                ],
                { encoding: 'utf8' },
            )
        },
    }
}
