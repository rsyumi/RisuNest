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
