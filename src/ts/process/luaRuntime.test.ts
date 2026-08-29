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
    const runtimeSource = await readFile(resolve(process.cwd(), 'src/ts/process/luaRuntime.ts'), 'utf8')
    const runtimeJavaScript = ts.transpileModule(runtimeSource, {
        compilerOptions: {
            module: ts.ModuleKind.ES2022,
            target: ts.ScriptTarget.ES2023,
        },
    }).outputText
    const runtimeUrl = `data:text/javascript;base64,${Buffer.from(runtimeJavaScript).toString('base64')}`
    const wasmoonUrl = pathToFileURL(resolve(process.cwd(), 'node_modules/wasmoon/dist/index.js')).href
    const childSource = `
        import { LuaFactory } from ${JSON.stringify(wasmoonUrl)}
        import * as luaRuntime from ${JSON.stringify(runtimeUrl)}
        const { runLuaSource } = luaRuntime
        ${body}
    `

    return await new Promise<{ code: number | null; stderr: string; stdout: string; timedOut: boolean }>(
        (resolveChild) => {
            const child = spawn(process.execPath, ['--input-type=module', '--eval', childSource], {
                stdio: ['ignore', 'pipe', 'pipe'],
            })
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
            child.once('exit', (code) => {
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

it('interrupts top-level infinite source and cleans up before running later source', async () => {
    const child = await runIsolatedWasmoon(`
        const factory = new LuaFactory()
        const engine = await factory.createEngine()
        const initialTop = engine.global.getTop()
        try {
            await runLuaSource(engine, 'bounded_success = 41', 25)
            if (engine.global.getTop() !== initialTop) throw new Error('success leaked a thread')

            let timedOut = false
            try {
                await runLuaSource(engine, 'while true do end', 25)
            } catch (error) {
                timedOut = /timeout/i.test(String(error))
            }
            if (!timedOut) throw new Error('infinite source did not time out')
            if (engine.global.getTop() !== initialTop) throw new Error('timeout leaked a thread')

            await runLuaSource(engine, 'bounded_success = bounded_success + 1', 25)
            if (engine.global.get('bounded_success') !== 42) throw new Error('later source did not run')
            if (engine.global.getTop() !== initialTop) throw new Error('later source leaked a thread')
            console.log('bounded-source-ok')
        } finally {
            engine.global.close()
        }
    `)

    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
    expect(child.stdout).toContain('bounded-source-ok')
})

it.each([
    { label: 'a successful run', runFailure: undefined },
    { label: 'a rejected run', runFailure: new Error('source execution failed') },
])('closes the child thread before removing it after $label', async ({ runFailure }) => {
    const calls: string[] = []
    const thread = {
        loadString: () => calls.push('load'),
        setTimeout: () => calls.push('setTimeout'),
        run: async () => {
            calls.push('run')
            if (runFailure) throw runFailure
        },
        close: () => calls.push('close'),
    }
    const engine = {
        global: {
            newThread: () => {
                calls.push('newThread')
                return thread
            },
            getTop: () => {
                calls.push('getTop')
                return 7
            },
            remove: (index: number) => calls.push(`remove:${index}`),
        },
    }
    const { runLuaSource } = await import('./luaRuntime')

    const pendingRun = runLuaSource(engine as never, 'return 1', 25)
    if (runFailure) {
        await expect(pendingRun).rejects.toBe(runFailure)
    } else {
        await expect(pendingRun).resolves.toBeUndefined()
    }
    expect(calls).toEqual([
        'newThread',
        'getTop',
        'load',
        'setTimeout',
        'run',
        'close',
        'remove:7',
    ])
})

it('sets the default source deadline to exactly 2,000 ms from now', async () => {
    const setThreadTimeout = vi.fn()
    const now = vi.spyOn(Date, 'now').mockReturnValue(10_000)
    const thread = {
        loadString: vi.fn(),
        setTimeout: setThreadTimeout,
        run: vi.fn().mockResolvedValue([]),
        close: vi.fn(),
    }
    const engine = {
        global: {
            newThread: () => thread,
            getTop: () => 3,
            remove: vi.fn(),
        },
    }
    const { runLuaSource } = await import('./luaRuntime')

    try {
        await runLuaSource(engine as never, 'return 1')
        expect(setThreadTimeout).toHaveBeenCalledOnce()
        expect(setThreadTimeout).toHaveBeenCalledWith(12_000)
    } finally {
        now.mockRestore()
    }
})

it('interrupts a Lua handler invoked from JavaScript through functionTimeout', async () => {
    const child = await runIsolatedWasmoon(`
        const factory = new LuaFactory()
        const engine = await factory.createEngine({ functionTimeout: 25 })
        try {
            await engine.doString('function loop_forever() while true do end end')
            const loopForever = engine.global.get('loop_forever')
            let timedOut = false
            try {
                await loopForever()
            } catch (error) {
                timedOut = /timeout/i.test(String(error))
            }
            if (!timedOut) throw new Error('handler did not time out')
            console.log('bounded-handler-ok')
        } finally {
            engine.global.close()
        }
    `)

    expect(child, child.stderr).toMatchObject({ code: 0, timedOut: false })
    expect(child.stdout).toContain('bounded-handler-ok')
})
