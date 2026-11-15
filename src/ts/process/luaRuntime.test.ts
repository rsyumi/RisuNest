// @vitest-environment node

import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import ts from 'typescript'
import { expect, it, vi } from 'vitest'

const runtimeState = vi.hoisted(() => ({
    factoryConstructions: 0,
    moduleEvaluations: 0,
}))

vi.mock('wasmoon', () => {
    runtimeState.moduleEvaluations++
    return {
        LuaFactory: class {
            constructor() {
                runtimeState.factoryConstructions++
            }
        },
    }
})

async function runIsolatedWasmoon(body: string) {
    const wasmoonUrl = pathToFileURL(resolve(process.cwd(), 'node_modules/wasmoon/dist/index.js')).href
    const budgetSource = await readFile(resolve(process.cwd(), 'src/ts/process/luaExecutionBudget.ts'), 'utf8')
    const toModuleUrl = (source: string) => {
        const javascript = ts.transpileModule(source, {
            compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2023 },
        }).outputText.replaceAll("'wasmoon'", JSON.stringify(wasmoonUrl))
            .replace("import wasmUrl from 'wasmoon/dist/glue.wasm?url';",
                `const wasmUrl = ${JSON.stringify(resolve('node_modules/wasmoon/dist/glue.wasm'))};`)
        return 'data:text/javascript;base64,' + Buffer.from(javascript).toString('base64')
    }
    const runtimeSource = await readFile(resolve(process.cwd(), 'src/ts/process/luaRuntime.ts'), 'utf8')
    const runtimeUrl = toModuleUrl(runtimeSource.replaceAll("'./luaExecutionBudget'", JSON.stringify(toModuleUrl(budgetSource))))
    const wrapperSource = await readFile(resolve(process.cwd(), 'src/ts/process/luaCodeWrapper.ts'), 'utf8')
    const jsonSource = await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8')
    const childSource = `
        import assert from 'node:assert/strict'
        import * as luaRuntime from ${JSON.stringify(runtimeUrl)}
        const { runLuaSource } = luaRuntime
        import { luaCodeWrapper } from ${JSON.stringify(toModuleUrl(wrapperSource))}
        const factory = await luaRuntime.createLuaFactory()
        await factory.mountFile('json.lua', ${JSON.stringify(jsonSource)})
        const engines = []
        async function create(timeout = 100) {
            const engine = await factory.createEngine({injectObjects: true, enableProxy: false, functionTimeout: timeout})
            engines.push(engine)
            return engine
        }
        const engine = await create()
        const unhandled = []
        process.on('unhandledRejection', error => unhandled.push(String(error)))
        try {
            ${body}
            for (const current of engines) {
                if (current.global.isClosed()) continue
                await runLuaSource(current, 'function healthy() return 42 end')
                assert.equal(current.global.get('healthy')(), 42)
            }
            await new Promise(resolve => setImmediate(resolve))
            assert.deepEqual(unhandled, [])
        } finally {
            for (const current of engines) current.global.close()
        }
        console.log('runtime-ok')
    `

    return await new Promise<{ code: number | null; stderr: string; stdout: string; timedOut: boolean }>(
        (resolveChild, rejectChild) => {
            const child = spawn(process.execPath, ['--input-type=module'], {
                stdio: ['pipe', 'pipe', 'pipe'],
            })
            child.stdin.end(childSource)
            let stderr = ''
            let stdout = ''
            let timedOut = false
            child.stderr.setEncoding('utf8')
            child.stdout.setEncoding('utf8')
            child.stderr.on('data', (chunk) => {
                stderr += chunk
            })
            child.stdout.on('data', (chunk) => {
                stdout += chunk
            })
            const timeout = setTimeout(() => {
                timedOut = true
                child.kill()
            }, 3_000)
            child.once('error', (error) => {
                clearTimeout(timeout)
                rejectChild(error)
            })
            child.once('close', (code) => {
                clearTimeout(timeout)
                resolveChild({ code, stderr, stdout, timedOut })
            })
        },
    )
}

it('loads Wasmoon only on the first explicit Lua factory request', async () => {
    const { createLuaFactory } = await import('./luaRuntime')
    expect(runtimeState.moduleEvaluations).toBe(0)
    expect(runtimeState.factoryConstructions).toBe(0)

    await createLuaFactory()
    expect(runtimeState.moduleEvaluations).toBe(1)
    expect(runtimeState.factoryConstructions).toBe(1)

    await createLuaFactory()
    expect(runtimeState.moduleEvaluations).toBe(1)
    expect(runtimeState.factoryConstructions).toBe(2)
})

it('shares a failed import with concurrent callers and retries after rejection', async () => {
    const { createLuaFactoryLoader } = await import('./luaRuntime')
    const failure = new Error('lazy runtime load failed')
    let rejectFirst!: (error: Error) => void
    const firstImport = new Promise<never>((_resolve, reject) => {
        rejectFirst = reject
    })
    const importRuntime = vi
        .fn()
        .mockReturnValueOnce(firstImport)
        .mockResolvedValueOnce({
            LuaFactory: class {},
        })
    const makeFactory = createLuaFactoryLoader(importRuntime)

    const firstCaller = makeFactory()
    const concurrentCaller = makeFactory()
    rejectFirst(failure)

    await expect(firstCaller).rejects.toBe(failure)
    await expect(concurrentCaller).rejects.toBe(failure)
    expect(importRuntime).toHaveBeenCalledTimes(1)

    await expect(makeFactory()).resolves.toBeInstanceOf(Object)
    expect(importRuntime).toHaveBeenCalledTimes(2)
})

it.each([
    ['initial source', `await assert.rejects(runLuaSource(engine, 'while true do end', 25), /thread timeout exceeded/)`],
    ['handler', `
        await runLuaSource(engine, 'function run() while true do end end')
        assert.throws(() => engine.global.get('run')(), /thread timeout exceeded/)
    `],
    ['async before await', `
        await runLuaSource(engine, luaCodeWrapper('run = async(function() while true do end end)'))
        await assert.rejects(async () => engine.global.get('run')(), /thread timeout exceeded/)
    `],
    ['async after await', `
        engine.global.set('boundary', () => Promise.resolve(true))
        await runLuaSource(engine, luaCodeWrapper('run = async(function() boundary():await(); while true do end end)'))
        await assert.rejects(async () => engine.global.get('run')(), /thread timeout exceeded/)
    `],
    ['wrapped coroutine', `
        await runLuaSource(engine, 'run = coroutine.wrap(function() coroutine.yield(7); while true do end end)')
        assert.equal(engine.global.get('run')(), 7)
        await create()
        assert.throws(() => engine.global.get('run')(), /thread timeout exceeded/)
    `],
])('interrupts an infinite %s and leaves later Lua work usable', async (_label, body) => {
    const child = await runIsolatedWasmoon(body)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
    expect(child.stdout).toContain('runtime-ok')
})

it.each(['success', 'error', 'timeout', 'syntax'])('releases the source thread after %s', async (outcome) => {
    const child = await runIsolatedWasmoon(`
        const initialTop = engine.global.getTop()
        const outcome = ${JSON.stringify(outcome)}
        const source = {success: 'value = 41', error: 'error("synthetic")', timeout: 'while true do end', syntax: 'function'}[outcome]
        if (outcome === 'success') await runLuaSource(engine, source, 25)
        else await assert.rejects(runLuaSource(engine, source, 25))
        assert.equal(engine.global.getTop(), initialTop)
        await runLuaSource(engine, 'value = 42')
        assert.equal(engine.global.get('value'), 42)
        assert.equal(engine.global.getTop(), initialTop)
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each(['create', 'wrap'])('preserves a coroutine.%s created during source evaluation across engine churn', async (kind) => {
    const child = await runIsolatedWasmoon(`
        await runLuaSource(engine,
            'saved = coroutine.${kind}(function() local total = 0; for i = 1,5000 do total = total + i end; return total end); ' +
            ${JSON.stringify(kind === 'create'
                ? 'function run() local ok, value = coroutine.resume(saved); if not ok then error(value) end; return value end'
                : 'function run() return saved() end')})
        await create()
        assert.equal(engine.global.get('run')(), 12502500)
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it('preserves nested async batches while other engines reuse function slots', async () => {
    const child = await runIsolatedWasmoon(`
        engine.global.set('boundary', () => new Promise(resolve => setImmediate(async () => {
            const other = await create()
            await runLuaSource(other, 'function unrelated() return 9 end')
            assert.equal(other.global.get('unrelated')(), 9)
            other.global.close()
            resolve(true)
        })))
        await runLuaSource(engine, luaCodeWrapper(\`
            local pipeline = async(function()
                boundary():await()
                local total = 0
                for i = 1,5000 do total = total + i end
                return total
            end)
            local batch = async(function()
                local tasks = {}
                for i = 1,5 do tasks[i] = pipeline() end
                local values = Promise.all(tasks):await()
                local total = 0
                for _, value in ipairs(values) do total = total + value end
                return total
            end)
            run = async(function() return batch():await() end)
        \`))
        assert.equal(await engine.global.get('run')(), 62512500)
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each(['handler', 'source'])('applies the %s deadline correctly across a long async wait', async (entry) => {
    const child = await runIsolatedWasmoon(`
        let now = 0
        const originalNow = performance.now
        performance.now = () => now
        try {
            engine.global.set('boundary', () => new Promise(resolve => setImmediate(() => {
                now += 61000
                resolve(true)
            })))
            const work = 'boundary():await(); local total = 0; for i = 1,5000 do total = total + i end; return total'
            if (${JSON.stringify(entry)} === 'source') {
                await assert.rejects(runLuaSource(engine, work), /thread timeout exceeded/)
            } else {
                await runLuaSource(engine, luaCodeWrapper('run = async(function() ' + work + ' end)'))
                assert.equal(await engine.global.get('run')(), 12502500)
            }
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each([4_999, 5_001])('uses a 5,000 ms default source budget (%i ms elapsed)', async (elapsed) => {
    const child = await runIsolatedWasmoon(`
        let now = 0
        const originalNow = performance.now
        performance.now = () => now
        engine.global.set('advance', () => { now += ${elapsed} })
        try {
            const run = runLuaSource(engine, 'advance(); local total = 0; for i = 1,5000 do total = total + i end; value = total')
            if (${elapsed} > 5000) await assert.rejects(run, /thread timeout exceeded/)
            else { await run; assert.equal(engine.global.get('value'), 12502500) }
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it('keeps the outer deadline during synchronous host reentry', async () => {
    const child = await runIsolatedWasmoon(`
        let now = 0
        const originalNow = performance.now
        performance.now = () => now
        engine.global.set('advance', () => { now += 60 })
        engine.global.set('reenter', () => engine.global.get('inner')())
        try {
            await runLuaSource(engine, \`
                function inner()
                    advance()
                    local total = 0
                    for i = 1,5000 do total = total + i end
                    return total
                end
                function run() advance(); return reenter() end
            \`)
            assert.throws(() => engine.global.get('run')(), /thread timeout exceeded/)
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each([
    ['plain', "error('synthetic failure')", 'synthetic failure'],
    ['custom', "error(setmetatable({}, {__tostring=function() return 'custom message' end}))", 'custom message'],
    ['throwing formatter', "error(setmetatable({}, {__tostring=function() error('format failed') end}))", 'format failed'],
    ['looping formatter', "error(setmetatable({}, {__tostring=function() while true do end end}))", 'thread timeout exceeded'],
].flatMap(([label, code, expected]) => ['handler', 'expired source'].map(entry => ({ label, code, expected, entry }))))(
    'reports a $label error from $entry without corrupting the runtime', async ({ label, code, expected, entry }) => {
        const child = await runIsolatedWasmoon(`
            const expected = new RegExp(${JSON.stringify(expected)})
            if (${JSON.stringify(entry)} === 'handler') {
                await runLuaSource(engine, ${JSON.stringify(`function run() ${code} end`)})
                assert.throws(() => engine.global.get('run')(), expected)
            } else {
                let now = 0
                const originalNow = performance.now
                performance.now = () => now
                engine.global.set('advance', () => { now += 6000 })
                try {
                    const initialTop = engine.global.getTop()
                    await assert.rejects(runLuaSource(engine, ${JSON.stringify(`local function fail() advance(); ${code} end; fail()`)}), error => {
                        assert.match(error.message, expected)
                        if (${JSON.stringify(label)} === 'plain') assert.match(error.message, /stack traceback:/)
                        return true
                    })
                    assert.equal(engine.global.getTop(), initialTop)
                } finally { performance.now = originalNow }
            }
        `)
        expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
    },
)

it('handles Promise rejection and late callbacks after closing their engine', async () => {
    const child = await runIsolatedWasmoon(`
        engine.global.set('boundary', () => Promise.reject(new Error('synthetic rejection')))
        await runLuaSource(engine, luaCodeWrapper('run = async(function() return boundary():await() end)'))
        await assert.rejects(async () => engine.global.get('run')(), /synthetic rejection/)
        const callback = engine.global.get('run')
        engine.global.close()
        engine.global.close()
        await create()
        assert.equal(callback(), undefined)
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it('releases engine callbacks after finalizers and keeps other engines usable', async () => {
    const child = await runIsolatedWasmoon(`
        const lua = await factory.getLuaModule()
        const allocated = new Set()
        const add = lua.module.addFunction
        const remove = lua.module.removeFunction
        lua.module.addFunction = (...args) => {
            const pointer = add(...args)
            allocated.add(pointer)
            return pointer
        }
        lua.module.removeFunction = pointer => { allocated.delete(pointer); remove(pointer) }
        let finalized = 0
        for (let i = 0; i < 20; i++) {
            const other = await create(10)
            other.global.set('finalized', () => { finalized++ })
            await runLuaSource(other, 'retained = setmetatable({}, {__gc = function() finalized() end})')
            other.global.close()
            other.global.close()
        }
        assert.equal(finalized, 20)
        assert.equal(allocated.size, 0)
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it('interrupts source that yields too often to reach the instruction hook', async () => {
    const child = await runIsolatedWasmoon(`
        await assert.rejects(runLuaSource(engine, 'while true do coroutine.yield() end', 25), /thread timeout exceeded/)
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each([
    ['pcall', 'pcall(longLoop)'],
    ['xpcall', 'xpcall(longLoop, function(failure) return failure end)'],
    ['coroutine.resume', 'coroutine.resume(coroutine.create(longLoop))'],
])('ends a %s retry loop at the first timeout it catches', async (_label, attempt) => {
    const child = await runIsolatedWasmoon(`
        const timedEngine = await create(300)
        let now = 0
        const originalNow = performance.now
        performance.now = () => now
        timedEngine.global.set('expire', () => { now = 1e12 })
        try {
            await runLuaSource(timedEngine, \`
                tries = 0
                local function longLoop() expire(); while true do end end
                function run()
                    local ok
                    repeat
                        tries = tries + 1
                        ok = ${attempt}
                    until ok or tries >= 1e6
                end
            \`)
            assert.throws(() => timedEngine.global.get('run')(), /thread timeout exceeded/)
            assert.equal(timedEngine.global.get('tries'), 1)
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it('runs a coroutine that caught a timeout at normal speed under its next deadline', async () => {
    const child = await runIsolatedWasmoon(`
        const lua = await factory.getLuaModule()
        let hookCalls = 0
        const add = lua.module.addFunction
        lua.module.addFunction = (callback, signature) => add(signature === 'vii'
            ? (...args) => { hookCalls++; return callback(...args) }
            : callback, signature)
        const timedEngine = await create(300)
        lua.module.addFunction = add
        let now = 0
        const originalNow = performance.now
        performance.now = () => now
        timedEngine.global.set('expire', () => { now = 1e12 })
        try {
            await runLuaSource(timedEngine, \`
                saved = coroutine.wrap(function()
                    local ok = coroutine.resume(coroutine.create(function() expire(); while true do end end))
                    coroutine.yield(ok)
                    local total = 0
                    for i = 1, 100000 do total = total + i end
                    coroutine.yield(total)
                end)
                function run() local value = saved(); return value end
            \`)
            assert.throws(() => timedEngine.global.get('run')(), /thread timeout exceeded/)
            now = 0
            hookCalls = 0
            assert.equal(timedEngine.global.get('run')(), 5000050000)
            // About 400,000 instructions: one hook call per 1,000, not one per instruction.
            assert.ok(hookCalls < 2000, String(hookCalls))
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each([5_000, 60_000])('honors a %i ms handler budget at either side of its deadline', async (timeoutMs) => {
    const child = await runIsolatedWasmoon(`
        const timedEngine = await create(${timeoutMs})
        let now = 0
        let elapsed = ${timeoutMs} - 1
        const originalNow = performance.now
        performance.now = () => now
        timedEngine.global.set('advance', () => { now += elapsed })
        try {
            await runLuaSource(timedEngine, 'function run() advance(); local n = 0; for i = 1,5000 do n = n + i end; return n end')
            assert.equal(timedEngine.global.get('run')(), 12502500)
            elapsed = ${timeoutMs} + 1
            assert.throws(() => timedEngine.global.get('run')(), /thread timeout exceeded/)
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})

it.each([
    ['one decode of an escape-heavy string', 'expire(); return #json.decode(heavy)'],
    ['a decode nested in a coroutine and pcall', 'return coroutine.wrap(function() return pcall(function() expire(); return #json.decode(heavy) end) end)()'],
])('reaches the timeout during %s and decodes normally afterwards', async (_label, body) => {
    const child = await runIsolatedWasmoon(`
        const timedEngine = await create(300)
        let now = 0
        const originalNow = performance.now
        performance.now = () => now
        timedEngine.global.set('expire', () => { now = 1e12 })
        timedEngine.global.set('heavy', JSON.stringify('"'.repeat(100000)))
        timedEngine.global.set('plain', JSON.stringify('x'.repeat(1000000)))
        try {
            await runLuaSource(timedEngine, ${JSON.stringify(`
                local json = require 'json'
                function run() ${body} end
                function check() return #json.decode(heavy) + #json.decode(plain) end
            `)})
            assert.throws(() => timedEngine.global.get('run')(), /thread timeout exceeded/)
            now = 0
            assert.equal(timedEngine.global.get('check')(), 1100000)
        } finally { performance.now = originalNow }
    `)
    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
})
