import { describe, expect, it, vi } from 'vitest'

import {
    createLocalPluginPermissionStore,
    createNativePluginPermissionStore,
} from './nativePluginPermissions'

function nativeBackend() {
    const permissions: { codeHash: string, permission: string, granted: boolean }[] = []
    const grants: { pluginName: string, permission: string, lastGrantAt: number }[] = []
    const invoked: string[] = []
    const invokeCommand = vi.fn(async (command: string, args?: Record<string, unknown>) => {
        invoked.push(command)
        if (command === 'pds_read_plugin_permissions') return { permissions, grants }
        if (command === 'pds_write_plugin_permission') {
            permissions.push(args as never)
            return undefined
        }
        if (command === 'pds_write_plugin_permission_grant') {
            grants.push(args as never)
            return undefined
        }
        throw new Error(`unexpected command ${command}`)
    })
    return { permissions, grants, invoked, invokeCommand }
}

describe('native plugin permissions', () => {
    it('reads the whole device state once and answers later checks from memory', async () => {
        const backend = nativeBackend()
        backend.permissions.push(
            { codeHash: 'hash-a', permission: 'db', granted: true },
            { codeHash: 'hash-b', permission: 'db', granted: false },
        )
        backend.grants.push({ pluginName: 'Plugin A', permission: 'db', lastGrantAt: 17 })
        const store = createNativePluginPermissionStore(backend.invokeCommand)

        await expect(store.isGranted('hash-a', 'db')).resolves.toBe(true)
        await expect(store.isGranted('hash-b', 'db')).resolves.toBe(false)
        await expect(store.isGranted('hash-a', 'mainDom')).resolves.toBe(false)
        await expect(store.lastGrantAt('Plugin A', 'db')).resolves.toBe(17)
        await expect(store.lastGrantAt('Plugin B', 'db')).resolves.toBeNull()

        expect(backend.invoked.filter((command) => command === 'pds_read_plugin_permissions'))
            .toHaveLength(1)
    })

    it('writes a grant and its reconfirmation time to separate commands', async () => {
        const backend = nativeBackend()
        const store = createNativePluginPermissionStore(backend.invokeCommand)

        await store.grant('hash-a', 'sendChat')
        await store.recordGrant('Plugin A', 'sendChat', 42)

        expect(backend.permissions).toEqual([
            { codeHash: 'hash-a', permission: 'sendChat', granted: true },
        ])
        expect(backend.grants).toEqual([
            { pluginName: 'Plugin A', permission: 'sendChat', lastGrantAt: 42 },
        ])
        await expect(store.isGranted('hash-a', 'sendChat')).resolves.toBe(true)
        await expect(store.lastGrantAt('Plugin A', 'sendChat')).resolves.toBe(42)
    })

    it('keeps the two key shapes apart so a name cannot stand in for a hash', async () => {
        const backend = nativeBackend()
        const store = createNativePluginPermissionStore(backend.invokeCommand)

        await store.recordGrant('hash-a', 'db', 7)

        await expect(store.isGranted('hash-a', 'db')).resolves.toBe(false)
    })

    it('opens the browser store only when the web build first uses it', async () => {
        const values = new Map<string, unknown>()
        const createInstance = vi.fn(() => ({
            getItem: async (key: string) => values.get(key) ?? null,
            setItem: async (key: string, value: unknown) => {
                values.set(key, value)
                return value
            },
        }) as unknown as LocalForage)
        const store = createLocalPluginPermissionStore(createInstance)
        expect(createInstance).not.toHaveBeenCalled()

        await store.grant('hash-a', 'db')
        await store.recordGrant('Plugin A', 'db', 9)

        expect(createInstance).toHaveBeenCalledTimes(1)
        expect(values.get('hash-a_db')).toBe(true)
        expect(values.get('Plugin A_db_lastGrantTime')).toBe(9)
        await expect(store.isGranted('hash-a', 'db')).resolves.toBe(true)
        await expect(store.lastGrantAt('Plugin A', 'db')).resolves.toBe(9)
    })
})
