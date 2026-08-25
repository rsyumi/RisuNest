import type { LuaFactory } from 'wasmoon'

let wasmoonModulePromise: Promise<typeof import('wasmoon')> | undefined

export async function createLuaFactory(): Promise<LuaFactory> {
    const { LuaFactory } = await (wasmoonModulePromise ??= import('wasmoon'))
    return new LuaFactory()
}
