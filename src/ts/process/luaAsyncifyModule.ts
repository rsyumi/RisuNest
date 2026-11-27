import wasmUrl from './lua/worker.wasm?url'

let compiledModule: Promise<WebAssembly.Module> | undefined

export async function loadLuaAsyncifyModule() {
    const pending = compiledModule ??= fetch(wasmUrl).then(async response => {
        if (!response.ok) throw new Error('Lua WASM could not be loaded')
        return WebAssembly.compile(await response.arrayBuffer())
    })
    try { return { url: wasmUrl, module: await pending } }
    catch (error) { if (compiledModule === pending) compiledModule = undefined; throw error }
}
