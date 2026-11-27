import { Worker as NodeWorker } from 'node:worker_threads'
import { readFile, mkdtemp, writeFile, unlink, rmdir } from 'node:fs/promises'
import { resolve, join } from 'node:path'
import { tmpdir } from 'node:os'
import { pathToFileURL } from 'node:url'
import ts from 'typescript'

/** Node's real message boundary, using the same worker entry and WASM as the browser. */
export async function nodeLuaWorkers(options: { failWasm?: boolean } = {}) {
    const directory = await mkdtemp(join(tmpdir(), 'risunest-lua-worker-'))
    const names = ['luaWorkerProtocol', 'luaExecutionBudget', 'luaAsyncifyModule', 'luaAsyncifyRuntime', 'luaWorker']
    const wasmoon = pathToFileURL(resolve('node_modules/wasmoon/dist/index.js')).href
    for (const name of names) {
        const source = await readFile(resolve(`src/ts/process/${name}.ts`), 'utf8')
        const output = ts.transpileModule(source, {
            compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2023 },
        }).outputText
            .replaceAll("'wasmoon'", JSON.stringify(wasmoon))
            .replace(/from '(\.\/lua\w+)'/g, "from '$1.mjs'")
            .replace("import wasmUrl from './lua/worker.wasm?url';",
                `const wasmUrl = ${JSON.stringify(resolve('src/ts/process/lua/worker.wasm'))};`)
        await writeFile(resolve(directory, `${name}.mjs`), output)
    }
    const bootstrap = resolve(directory, 'bootstrap.mjs')
    await writeFile(bootstrap, `import {parentPort} from 'node:worker_threads';
        import {readFile} from 'node:fs/promises';
        globalThis.self = globalThis;
        globalThis.postMessage = message => parentPort.postMessage(message);
        globalThis.fetch = async url => {
            if (url !== ${JSON.stringify(resolve('src/ts/process/lua/worker.wasm'))}) throw new Error('Unexpected Worker fetch');
            return new Response(${options.failWasm ? "'unavailable', {status: 503}" : 'await readFile(url)'});
        };
        await import('./luaWorker.mjs');
        parentPort.on('message', data => self.onmessage({data}));`)
    const running = new Set<NodeWorker>()
    class WorkerBoundary {
        onmessage?: (event: { data: unknown }) => void
        onerror?: (event: { message: string; preventDefault(): void }) => void
        onmessageerror?: () => void
        private readonly worker: NodeWorker
        constructor() {
            this.worker = new NodeWorker(pathToFileURL(bootstrap))
            running.add(this.worker)
            this.worker.on('message', data => this.onmessage?.({ data }))
            this.worker.on('error', error => this.onerror?.({ message: error.message, preventDefault() {} }))
            this.worker.on('messageerror', () => this.onmessageerror?.())
            this.worker.on('exit', () => running.delete(this.worker))
        }
        postMessage(message: unknown): void { this.worker.postMessage(message) }
        terminate(): void { void this.worker.terminate() }
    }
    return { Worker: WorkerBoundary, close: async () => {
        await Promise.all([...running].map(worker => worker.terminate()))
        await Promise.all([...names, 'bootstrap'].map(name => unlink(resolve(directory, `${name}.mjs`))))
        await rmdir(directory)
    } }
}
