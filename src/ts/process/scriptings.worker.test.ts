// @vitest-environment node

import { readFile } from 'node:fs/promises'
import { afterAll, beforeAll, expect, test, vi } from 'vitest'
import { nodeLuaWorkers } from './tests/luaWorkerNode'
import type { Chat } from '../storage/database.svelte'
import { LuaWorkerExecutor } from './luaWorkerExecutor'
import { luaCodeWrapper } from './luaCodeWrapper'
import { LuaDeferredReply, type LuaWorkerResponse } from './luaWorkerProtocol'

vi.mock('../storage/persistentDataRuntime.svelte', async () => (await import('./tests/scriptingsTestHarness')).persistentRuntimeModule())
vi.mock('../parser/parser.svelte', async () => (await import('./tests/scriptingsTestHarness')).parserModule())
vi.mock('../alert', async () => (await import('./tests/scriptingsTestHarness')).alertModule())
vi.mock('../globalApi.svelte', () => ({ fetchNative: vi.fn(), readImage: vi.fn() }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'android' }))
vi.mock('../tokenizer', async () => (await import('./tests/scriptingsTestHarness')).tokenizerModule())
vi.mock('../util', async () => (await import('./tests/scriptingsTestHarness')).utilModule())
vi.mock('../storage/database.svelte', async () => (await import('./tests/scriptingsTestHarness')).databaseModule())
vi.mock('../stores.svelte', async () => (await import('./tests/scriptingsTestHarness')).storesModule())
vi.mock('./modules', async () => (await import('./tests/scriptingsTestHarness')).modulesModule())
vi.mock('./files/inlays', async () => (await import('./tests/scriptingsTestHarness')).inlaysModule())
vi.mock('./lorebook.svelte', async () => (await import('./tests/scriptingsTestHarness')).lorebookModule())
vi.mock('./memory/hypamemory', async () => (await import('./tests/scriptingsTestHarness')).hypamemoryModule())
vi.mock('./request/request', async () => (await import('./tests/scriptingsTestHarness')).requestModule())
vi.mock('./stableDiff', async () => (await import('./tests/scriptingsTestHarness')).stableDiffModule())
vi.mock('./command', async () => (await import('./tests/scriptingsTestHarness')).commandModule())

let runScripted: typeof import('./scriptings').runScripted
let workers: Awaited<ReturnType<typeof nodeLuaWorkers>>
beforeAll(async () => {
    workers = await nodeLuaWorkers()
    vi.stubGlobal('Worker', workers.Worker)
    const json = await readFile('public/lua/json.lua', 'utf8')
    vi.stubGlobal('fetch', vi.fn(async () => new Response(json)))
    runScripted = (await import('./scriptings')).runScripted
})
afterAll(async () => { await workers.close(); vi.unstubAllGlobals() })

test('answers a fresh watchdog probe without requiring a Lua engine', async () => {
    const worker = new workers.Worker()
    try {
        const response = new Promise<LuaWorkerResponse>(resolve => {
            worker.onmessage = event => resolve(event.data as LuaWorkerResponse)
        })
        worker.postMessage({ engine: 0, command: { kind: 'ping', token: 7 } })
        expect(await response).toEqual({ engine: 0, event: { kind: 'pong', token: 7 } })
    } finally { worker.terminate() }
})

test('runs real host APIs in C callbacks, preserves independent views and read-after-write', async () => {
    const chat = { message: [{ role: 'user', data: 'before', chatId: 'a' }] } as Chat
    const result = await runScripted(`function onStart(id)
        local a = getFullChat(id)
        a[1].data = 'local'
        assert(getFullChat(id)[1].data == 'before')
        local first = ('x'):gsub('x', function() return getChatData(id, 0) end)
        setChat(id, 0, 'after')
        return first .. ':' .. getFullChat(id)[1].data
    end`, { char: { chaId: 'worker-read-write' } as never, chat, mode: 'start' })
    expect(result.res).toBe('before:after')
    expect(chat.message[0].data).toBe('after')
})

test('preserves explicit false, nil, missing handlers and retained globals', async () => {
    const arg = { char: { chaId: 'worker-results' } as never, chat: { message: [] } as Chat, mode: 'start' }
    const code = 'counter=0; function onStart() counter=counter+1; return counter end'
    expect((await runScripted(code, arg)).res).toBe(1)
    expect((await runScripted(code, arg)).res).toBe(2)
    expect((await runScripted(code, { ...arg, char: { chaId: 'worker-other' } as never })).res).toBe(1)
    expect((await runScripted('function onStart() return false end', arg)).stopSending).toBe(true)
    expect((await runScripted('function onStart() end', arg)).res).toBeNull()
    expect((await runScripted('', arg)).res).toBeUndefined()
})

test('retains the existing async host shape and display write restrictions', async () => {
    const chat = { message: [{ role: 'user', data: 'before' }] } as Chat
    const vars = new Map<string, string>()
    const result = await runScripted(`listenEdit('editDisplay', function(id, text)
        setChat(id, 0, 'forbidden')
        setState(id, 'x', {n=1})
        return text .. ':' .. getState(id, 'x').n .. ':' .. getChatData(id, 0)
    end)`, { char: { chaId: 'worker-display' } as never, chat, data: 'display', mode: 'editDisplay',
        setVar: (key, value) => { vars.set(key, value) }, getVar: key => vars.get(key) ?? 'null' })
    expect(result.res).toBe('display:1:before')
    expect(chat.message[0].data).toBe('before')
    const waited = await runScripted(`onOutput=async(function(id)
        sleep(id, 1):await()
        return getChatData(id, 0)
    end)`, { char: { chaId: 'worker-async' } as never, chat, mode: 'output' })
    expect(waited.res).toBe('before')
})

test('does not renew a handler deadline on synchronous RPC and retains globals after timeout', async () => {
    const executor = new LuaWorkerExecutor(30)
    executor.register('ping', () => 1)
    try {
        await executor.initialize(luaCodeWrapper('count=7; function run() count=count+1; while true do ping() end end; function read() return count end'), await readFile('public/lua/json.lua', 'utf8'))
        await expect(executor.invoke('run', [])).rejects.toThrow(/thread timeout exceeded/)
        expect(executor.isClosed()).toBe(false)
        expect(await executor.invoke('read', [])).toBe(8)
    } finally { executor.close() }
}, 10_000)

test.each(['async API', 'synchronous API in a C callback'] as const)('excludes a pending host wait from the handler deadline (%s)', async kind => {
    const executor = new LuaWorkerExecutor(30)
    executor.register('wait', () => {
        const pending = new Promise<string>(resolve => setTimeout(() => resolve('completed'), 180))
        return kind === 'async API' ? pending : new LuaDeferredReply(pending)
    })
    try {
        const source = kind === 'async API'
            ? 'onOutput=async(function() return wait():await() end)'
            : "function onOutput() return ('x'):gsub('x', function() return wait() end) end"
        await executor.initialize(luaCodeWrapper(source), await readFile('public/lua/json.lua', 'utf8'))
        expect(await executor.invoke('onOutput', [])).toBe('completed')
    } finally { executor.close() }
})

test('finishes an aborted invocation without replaying effects or discarding the engine', async () => {
    const executor = new LuaWorkerExecutor(1_000)
    const other = new LuaWorkerExecutor(1_000)
    const writes: string[] = []
    let finish!: () => void
    let started!: () => void
    const ready = new Promise<void>(resolve => { started = resolve })
    executor.register('write', value => { writes.push(value) })
    executor.register('wait', () => { started(); return new Promise<void>(resolve => { finish = resolve }) })
    const controller = new AbortController()
    try {
        await other.initialize('count=0; function run() count=count+1; return count end', await readFile('public/lua/json.lua', 'utf8'))
        expect(await other.invoke('run', [])).toBe(1)
        await executor.initialize(luaCodeWrapper(`onOutput=async(function()
            write('before'); wait():await(); write('after')
        end)`), await readFile('public/lua/json.lua', 'utf8'))
        const result = executor.invoke('onOutput', [], controller.signal)
        const failed = expect(result).rejects.toThrow('cancelled')
        await ready
        controller.abort(new Error('cancelled'))
        finish()
        await failed
        expect(executor.isClosed()).toBe(false)
        expect(writes).toEqual(['before', 'after'])
        expect(await other.invoke('run', [])).toBe(2)
        expect(other.isClosed()).toBe(false)
    } finally { executor.close(); other.close() }
})

test('does not let a detached old Promise callback access a later invocation', async () => {
    const executor = new LuaWorkerExecutor(1_000)
    const writes: string[] = []
    let finish!: () => void
    executor.register('write', value => { writes.push(value) })
    executor.register('wait', () => new Promise<void>(resolve => { finish = resolve }))
    try {
        await executor.initialize(luaCodeWrapper(`
            function detach()
                pending=wait():next(function() write('stale') end):catch(function() end)
                return 'detached'
            end
            onOutput=async(function()
                local ok=pcall(function() pending:await() end)
                write('current')
                return ok
            end)
        `), await readFile('public/lua/json.lua', 'utf8'))
        expect(await executor.invoke('detach', [])).toBe('detached')
        const result = executor.invoke('onOutput', [])
        finish()
        await result
        expect(writes).toEqual(['current'])
    } finally { executor.close() }
})

test('keeps pure Promise continuations retained across invocations', async () => {
    const executor = new LuaWorkerExecutor(1_000)
    let finish!: (value: string) => void
    executor.register('wait', () => new Promise<string>(resolve => { finish = resolve }))
    try {
        await executor.initialize(luaCodeWrapper(`
            function detach()
                pending=wait():next(function(value) return value..':continued' end)
                return 'detached'
            end
            onOutput=async(function() return pending:await() end)
        `), await readFile('public/lua/json.lua', 'utf8'))
        expect(await executor.invoke('detach', [])).toBe('detached')
        const result=executor.invoke('onOutput', [])
        finish('original')
        expect(await result).toBe('original:continued')
    } finally { executor.close() }
})

test('formats error objects through synchronous host calls inside the protected Lua execution', async () => {
    const executor = new LuaWorkerExecutor(1_000)
    executor.register('describe', () => 'synthetic error')
    try {
        await executor.initialize(luaCodeWrapper(`
            local failure=setmetatable({}, {__tostring=function() return describe() end})
            function direct() error(failure) end
            onOutput=async(function()
                return Promise.resolve('ready'):next(function() error(failure) end):await()
            end)
        `), await readFile('public/lua/json.lua', 'utf8'))
        await expect(executor.invoke('direct', [])).rejects.toThrow('synthetic error')
        await expect(executor.invoke('onOutput', [])).rejects.toThrow('synthetic error')
    } finally { executor.close() }
}, 10_000)

test('keeps host Error userdata and primitive rejection values compatible', async () => {
    const executor = new LuaWorkerExecutor(1_000)
    executor.register('fail', () => { throw new Error('host problem') })
    executor.register('reject', () => Promise.reject('primitive rejection'))
    try {
        await executor.initialize(luaCodeWrapper(`
            function direct()
                local ok, problem=pcall(fail)
                return {ok=ok, message=problem.message, hidden=problem.constructor==null}
            end
            onOutput=async(function() return reject():await() end)
        `), await readFile('public/lua/json.lua', 'utf8'))
        expect(await executor.invoke('direct', [])).toEqual({ok:false, message:'host problem', hidden:true})
        await expect(executor.invoke('onOutput', [])).rejects.toBe('primitive rejection')
    } finally { executor.close() }
})

test('propagates product cancellation after the existing Lua invocation finishes', async () => {
    const { alertInput } = await import('../alert')
    let finish!: (value: string) => void
    let started!: () => void
    const ready = new Promise<void>(resolve => { started = resolve })
    vi.mocked(alertInput).mockImplementationOnce(() => {
        started()
        return new Promise<string>(resolve => { finish = resolve })
    })
    const chat = { message: [{ role: 'user', data: 'original' }] } as Chat
    const controller = new AbortController()
    const result = runScripted(`onOutput=async(function(id)
        setChat(id, 0, 'before')
        alertInput(id, 'synthetic'):await()
        setChat(id, 0, 'after')
    end)`, { char: { chaId: 'worker-product-abort' } as never, chat, mode: 'output', signal: controller.signal })
    const rejected = expect(result).rejects.toThrow('cancelled')
    await ready
    controller.abort(new Error('cancelled'))
    finish('late')
    await rejected
    expect(chat.message[0].data).toBe('after')
})

test('keeps display globals when a newer render cancels an in-flight invocation', async () => {
    const controller = new AbortController()
    const code = `counter=0
        listenEdit('editDisplay', function(id, text)
            counter=counter+1
            if text=='slow' then getChatVar(id, 'gate') end
            return text..':'..counter
        end)`
    const arg = { char: { chaId: 'worker-render-cancel' } as never, chat: { message: [] } as Chat, mode: 'editDisplay',
        getVar: () => { controller.abort(new Error('superseded render')); return 'gate' } }
    expect((await runScripted(code, { ...arg, data: 'a' })).res).toBe('a:1')
    const aborted = runScripted(code, { ...arg, data: 'slow', signal: controller.signal })
    await expect(aborted).rejects.toThrow('superseded render')
    expect((await runScripted(code, { ...arg, data: 'b' })).res).toBe('b:3')
})

test('does not charge another engine computation to a synchronous host caller', async () => {
    const a = new LuaWorkerExecutor(100)
    const b = new LuaWorkerExecutor(2_000)
    let other!: Promise<unknown>
    a.register('ping', () => { other = b.invoke('run', []); return 'pong' })
    try {
        const json = await readFile('public/lua/json.lua', 'utf8')
        await a.initialize('function run() return ping() end', json)
        await b.initialize('function run() local t=os.clock(); while os.clock()-t<0.4 do end; return "done" end', json)
        const result = a.invoke('run', [])
        await expect(result).resolves.toBe('pong')
        expect(await other).toBe('done')
        expect(a.isClosed()).toBe(false)
    } finally { a.close(); b.close() }
})

test('recovers on the next request after a Worker cannot run its instruction hook', async () => {
    const executor = new LuaWorkerExecutor(30)
    const waiting = new LuaWorkerExecutor(1_000)
    const preparing = new LuaWorkerExecutor(1_000)
    const writes: string[] = []
    preparing.register('write', value => { writes.push(value); return 'prepared' })
    let started!: () => void
    let finish!: () => void
    const ready = new Promise<void>(resolve => { started = resolve })
    waiting.register('wait', () => { started(); return new Promise<void>(resolve => { finish = resolve }) })
    try {
        await waiting.initialize(luaCodeWrapper('onOutput=async(function() return wait():await() end)'), await readFile('public/lua/json.lua', 'utf8'))
        const waitingResult = waiting.invoke('onOutput', [])
        const collateral = expect(waitingResult).rejects.toThrow('another engine')
        await ready
        await executor.initialize('function run() debug.sethook(); while true do end end', await readFile('public/lua/json.lua', 'utf8'))
        const failed = expect(executor.invoke('run', [])).rejects.toThrow('thread timeout exceeded')
        const recovered = expect(preparing.initialize('value=write("once"); function run() return value end',
            await readFile('public/lua/json.lua', 'utf8'))).resolves.toBeUndefined()
        await Promise.all([failed, collateral, recovered])
        expect(await preparing.invoke('run', [])).toBe('prepared')
        expect(writes).toEqual(['once'])
        expect(executor.isClosed()).toBe(true)
        const next = new LuaWorkerExecutor(1_000)
        try {
            await next.initialize('function run() return "recovered" end', await readFile('public/lua/json.lua', 'utf8'))
            expect(await next.invoke('run', [])).toBe('recovered')
        } finally { next.close() }
    } finally { finish?.(); waiting.close(); preparing.close(); executor.close() }
}, 15_000)

test('charges synchronous main-thread API work and keeps globals after the budget expires', async () => {
    const executor = new LuaWorkerExecutor(30)
    executor.register('slow', () => { const started=performance.now(); while (performance.now()-started<80) {} })
    try {
        await executor.initialize('count=0; function run() count=count+1; slow() end; function read() return count end', await readFile('public/lua/json.lua', 'utf8'))
        await expect(executor.invoke('run', [])).rejects.toThrow('thread timeout exceeded')
        expect(executor.isClosed()).toBe(false)
        expect(await executor.invoke('read', [])).toBe(1)
    } finally { executor.close() }
})

test.each(['constructor', 'load', 'wasm'])('falls back before user code when Worker %s fails', async failure => {
    const unavailable = failure === 'wasm' ? await nodeLuaWorkers({ failWasm: true }) : undefined
    class FailedWorker {
        onerror?: (event: { message: string; preventDefault(): void }) => void
        constructor() {
            if (failure === 'constructor') throw new Error('synthetic constructor failure')
            queueMicrotask(() => this.onerror?.({ message: 'synthetic load failure', preventDefault() {} }))
        }
        postMessage() {}
        terminate() {}
    }
    vi.resetModules()
    vi.stubGlobal('Worker', unavailable?.Worker ?? FailedWorker)
    try {
        const run = (await import('./scriptings')).runScripted
        const code = 'count=0; function onStart() count=count+1; return count end'
        const arg = { char: { chaId: `worker-startup-${failure}` } as never, chat: { message: [] } as Chat, mode: 'start' }
        expect((await run(code, arg)).res).toBe(1)
        expect((await run(code, arg)).res).toBe(2)
    } finally {
        await unavailable?.close()
        vi.stubGlobal('Worker', workers.Worker)
        vi.resetModules()
        runScripted = (await import('./scriptings')).runScripted
    }
})

test('does not replay a failing source through the fallback after a host API has run', async () => {
    let reads = 0
    const chat = { message: [{ role: 'user', get data() { reads++; return 'synthetic' } }] } as Chat
    await expect(runScripted('getFullChat("unused"); error("synthetic source failure")', {
        char: { chaId: 'worker-no-source-replay' } as never, chat, mode: 'start',
    })).rejects.toThrow('synthetic source failure')
    expect(reads).toBe(1)
})

test('excludes another engine computation from the five-second source budget', async () => {
    const a = new LuaWorkerExecutor(1_000)
    const b = new LuaWorkerExecutor(10_000)
    let other!: Promise<unknown>
    a.register('ping', () => { other = b.invoke('run', []); return 'pong' })
    try {
        const json = await readFile('public/lua/json.lua', 'utf8')
        await b.initialize('function run() local t=os.clock(); while os.clock()-t<5.2 do end; return "done" end', json)
        await a.initialize('value=ping(); function read() return value end', json)
        expect(await other).toBe('done')
        expect(await a.invoke('read', [])).toBe('pong')
    } finally { a.close(); b.close() }
}, 15_000)

test('still limits genuine asynchronous waits during source initialization to five seconds', async () => {
    const executor = new LuaWorkerExecutor(60_000)
    executor.register('wait', () => new Promise(resolve => setTimeout(resolve, 5_200)))
    try {
        await expect(executor.initialize(luaCodeWrapper('wait():await()'), await readFile('public/lua/json.lua', 'utf8')))
            .rejects.toThrow('thread timeout exceeded')
    } finally { executor.close() }
}, 15_000)
