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
