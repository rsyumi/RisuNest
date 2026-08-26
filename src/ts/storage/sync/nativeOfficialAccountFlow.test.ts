import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
    createNativeAppKvStringStorage,
    type NativeAppKv,
} from '../nativeAppKv'
import { createAccountScopedOfficialAssetLedger } from './officialAssetLedger'
import type { OfficialPullResult } from './officialAccountSnapshot'
import {
    createNativeOfficialAccountFlow,
    nativeOfficialAccountKeys,
} from './nativeOfficialAccountFlow'

function createHarness(loggedIn = false, useNativeRestore = false) {
    const values = new Map<string, unknown>()
    const events: string[] = []
    const appKv: NativeAppKv = {
        get: vi.fn(async (key: string) => values.get(key) ?? null),
        set: vi.fn(async (key, value) => {
            events.push(`set:${key}`)
            values.set(key, value)
        }),
        remove: vi.fn(async (key) => {
            events.push(`remove:${key}`)
            values.delete(key)
        }),
    }
    const publication = {
        publish: vi.fn(async () => { events.push('publish') }),
        dispose: vi.fn(async () => { events.push('dispose') }),
    }
    const adapter = {
        pull: vi.fn(async (): Promise<OfficialPullResult> => ({ kind: 'missing' })),
        pin: vi.fn(async () => publication),
        resetAccountAssociation: vi.fn(),
    }
    const flushPendingData = vi.fn(async () => { events.push('flush') })
    const restart = vi.fn(async () => { events.push('restart') })
    const setRouting = vi.fn((credential) => {
        events.push(credential ? 'route:set' : 'route:clear')
    })
    const clearLegacyFallback = vi.fn(() => events.push('fallback:clear'))
    const flushMetadata = vi.fn(async () => { events.push('metadata:flush') })
    const resetMetadata = vi.fn(() => events.push('metadata:reset'))
    const resetAccountSession = vi.fn(() => events.push('session:reset'))
    const nativeRestore = vi.fn(async (): Promise<
        OfficialPullResult | { kind: 'compatibility-fallback' }
    > => ({
        kind: 'activated',
        revision: 8,
    }))
    const flow = createNativeOfficialAccountFlow({
        appKv,
        adapter,
        initialCredential: loggedIn ? {
            id: 'account-1',
            token: 'legacy-token',
            data: {},
        } : null,
        flushPendingData,
        getRevision: () => 7,
        restart,
        setRouting,
        clearLegacyFallback,
        flushMetadata,
        resetMetadata,
        resetAccountSession,
        ...(useNativeRestore ? { nativeRestore } : {}),
    })
    return {
        adapter,
        appKv,
        clearLegacyFallback,
        events,
        flow,
        flushMetadata,
        flushPendingData,
        publication,
        nativeRestore,
        restart,
        setRouting,
        values,
    }
}

describe('explicit native official account flow', () => {
    beforeEach(() => vi.restoreAllMocks())

    it('persists only the legacy credential fields without pulling or publishing', async () => {
        const harness = createHarness()

        await expect(harness.flow.login({
            id: 'account-1',
            token: 'legacy-token',
            data: {
                refresh_token: 'refresh',
                access_token: 'access',
                expires_in: 123,
                dpop_private_key: 'must-not-persist',
            },
            useSync: true,
        } as never)).resolves.toEqual({
            id: 'account-1',
            token: 'legacy-token',
            data: {
                refresh_token: 'refresh',
                access_token: 'access',
                expires_in: 123,
            },
        })

        expect(harness.values.get(nativeOfficialAccountKeys.credential)).toEqual({
            id: 'account-1',
            token: 'legacy-token',
            data: {
                refresh_token: 'refresh',
                access_token: 'access',
                expires_in: 123,
            },
        })
        expect(harness.adapter.pull).not.toHaveBeenCalled()
        expect(harness.adapter.pin).not.toHaveBeenCalled()
        expect(harness.setRouting).toHaveBeenCalledOnce()
    })

    it('sanitizes and persists credentials returned by native reauthentication', async () => {
        const harness = createHarness(true)

        await expect(harness.flow.reauthenticate(JSON.stringify({
            id: 'account-1',
            token: 'refreshed-token',
            data: {
                refresh_token: 'refresh-2',
                access_token: 'access-2',
                expires_in: 456,
                dpop_private_key: 'must-not-persist',
            },
            useSync: true,
        }))).resolves.toEqual({
            id: 'account-1',
            token: 'refreshed-token',
            data: {
                refresh_token: 'refresh-2',
                access_token: 'access-2',
                expires_in: 456,
            },
        })

        expect(harness.values.get(nativeOfficialAccountKeys.credential)).toEqual({
            id: 'account-1',
            token: 'refreshed-token',
            data: {
                refresh_token: 'refresh-2',
                access_token: 'access-2',
                expires_in: 456,
            },
        })
        expect(harness.setRouting).toHaveBeenCalledWith({
            id: 'account-1',
            token: 'refreshed-token',
            data: {
                refresh_token: 'refresh-2',
                access_token: 'access-2',
                expires_in: 456,
            },
        })
    })

    it('resets adapter association when login changes accounts', async () => {
        const harness = createHarness(true)

        await harness.flow.login({
            id: 'account-2',
            token: 'other-token',
            data: {},
        })

        expect(harness.adapter.resetAccountAssociation).toHaveBeenCalledWith('account-2')
    })

    it('rolls back persisted and routed credentials when login routing fails', async () => {
        const harness = createHarness(true)
        const previous = {
            id: 'account-1',
            token: 'legacy-token',
            data: {},
        }
        harness.values.set(nativeOfficialAccountKeys.credential, previous)
        harness.setRouting.mockRejectedValueOnce(new Error('route failed'))

        await expect(harness.flow.login({
            id: 'account-2',
            token: 'other-token',
            data: {},
        })).rejects.toThrow('route failed')

        expect(harness.flow.getToken()).toBe('legacy-token')
        expect(harness.values.get(nativeOfficialAccountKeys.credential)).toEqual(previous)
        expect(harness.setRouting).toHaveBeenLastCalledWith(previous)
        expect(harness.adapter.resetAccountAssociation).toHaveBeenCalledWith('account-1')
    })

    it('flushes, pulls once, persists metadata, and cold-restarts after activation', async () => {
        const harness = createHarness(true)
        harness.adapter.pull.mockImplementationOnce(async () => {
            harness.events.push('pull')
            return { kind: 'activated', revision: 8 }
        })

        await expect(harness.flow.restore()).resolves.toEqual({
            kind: 'activated',
            revision: 8,
        })

        expect(harness.adapter.pull).toHaveBeenCalledOnce()
        expect(harness.adapter.pin).not.toHaveBeenCalled()
        expect(harness.events).toEqual([
            'flush',
            'pull',
            'metadata:flush',
            'restart',
        ])
    })

    it('still cold-restarts after activated restore metadata persistence fails', async () => {
        const harness = createHarness(true)
        harness.adapter.pull.mockImplementationOnce(async () => {
            harness.events.push('pull')
            return { kind: 'activated', revision: 8 }
        })
        harness.flushMetadata.mockImplementationOnce(async () => {
            harness.events.push('metadata:flush')
            throw new Error('metadata flush failed')
        })
        harness.restart.mockImplementationOnce(async () => {
            harness.events.push('restart')
            throw new Error('restart failed')
        })

        await expect(harness.flow.restore()).rejects.toThrow('metadata flush failed')

        expect(harness.events).toEqual([
            'flush',
            'pull',
            'metadata:flush',
            'restart',
        ])
    })

    it('requires login and never turns a missing restore into a publish', async () => {
        const loggedOut = createHarness()
        await expect(loggedOut.flow.restore()).rejects.toThrow(
            'Native official account login is required',
        )
        expect(loggedOut.flushPendingData).not.toHaveBeenCalled()
        expect(loggedOut.adapter.pull).not.toHaveBeenCalled()

        const loggedIn = createHarness(true)
        await expect(loggedIn.flow.restore()).resolves.toEqual({ kind: 'missing' })
        expect(loggedIn.adapter.pull).toHaveBeenCalledOnce()
        expect(loggedIn.adapter.pin).not.toHaveBeenCalled()
        expect(loggedIn.restart).not.toHaveBeenCalled()
    })

    it('uses the bounded native snapshot restore when it is configured', async () => {
        const harness = createHarness(true, true)

        await expect(harness.flow.restore()).resolves.toEqual({
            kind: 'activated',
            revision: 8,
        })

        expect(harness.nativeRestore).toHaveBeenCalledWith({
            id: 'account-1',
            token: 'legacy-token',
            data: {},
        })
        expect(harness.adapter.pull).not.toHaveBeenCalled()
        expect(harness.events).toEqual(['flush', 'metadata:flush', 'restart'])
    })

    it('uses the existing prepared Web restore only for a native compatibility fallback', async () => {
        const harness = createHarness(true, true)
        harness.nativeRestore.mockResolvedValueOnce({ kind: 'compatibility-fallback' })
        harness.adapter.pull.mockResolvedValueOnce({ kind: 'activated', revision: 9 })

        await expect(harness.flow.restore()).resolves.toEqual({
            kind: 'activated',
            revision: 9,
        })

        expect(harness.nativeRestore).toHaveBeenCalledOnce()
        expect(harness.adapter.pull).toHaveBeenCalledOnce()
        expect(harness.events).toEqual(['flush', 'metadata:flush', 'restart'])
    })

    it('serializes concurrent restore, publish, and logout operations', async () => {
        const harness = createHarness(true)
        let resolvePull: (result: OfficialPullResult) => void = () => undefined
        harness.adapter.pull.mockImplementationOnce(async () => {
            harness.events.push('pull')
            return new Promise<OfficialPullResult>((resolve) => {
                resolvePull = resolve
            })
        })

        const restore = harness.flow.restore()
        await vi.waitFor(() => expect(harness.adapter.pull).toHaveBeenCalledOnce())
        const publish = harness.flow.publish()
        const logout = harness.flow.logout()
        await new Promise((resolve) => setTimeout(resolve, 0))

        expect(harness.adapter.pin).not.toHaveBeenCalled()
        expect(harness.appKv.remove).not.toHaveBeenCalled()

        resolvePull({ kind: 'missing' })
        await expect(Promise.all([restore, publish, logout])).resolves.toEqual([
            { kind: 'missing' },
            undefined,
            undefined,
        ])
        expect(harness.adapter.pin).toHaveBeenCalledOnce()
        expect(harness.appKv.remove).toHaveBeenCalledTimes(3)
        expect(harness.events.indexOf('publish')).toBeLessThan(
            harness.events.indexOf('fallback:clear'),
        )
    })

    it('flushes, pins the current revision, publishes, and always disposes', async () => {
        const harness = createHarness(true)
        const signal = new AbortController().signal

        await harness.flow.publish(signal)

        expect(harness.adapter.pin).toHaveBeenCalledWith(7)
        expect(harness.publication.publish).toHaveBeenCalledWith(signal)
        expect(harness.events).toEqual(['flush', 'publish', 'dispose', 'metadata:flush'])

        harness.events.length = 0
        harness.publication.publish.mockRejectedValueOnce(new Error('offline'))
        harness.publication.dispose.mockImplementationOnce(async () => {
            harness.events.push('dispose')
            throw new Error('dispose failed')
        })
        harness.flushMetadata.mockImplementationOnce(async () => {
            harness.events.push('metadata:flush')
            throw new Error('metadata flush failed')
        })
        await expect(harness.flow.publish()).rejects.toThrow('offline')
        expect(harness.events).toEqual(['flush', 'dispose', 'metadata:flush'])
    })

    it('flushes metadata after pin fails and preserves the pin error', async () => {
        const harness = createHarness(true)
        harness.adapter.pin.mockImplementationOnce(async () => {
            harness.events.push('pin')
            throw new Error('pin failed')
        })
        harness.flushMetadata.mockImplementationOnce(async () => {
            harness.events.push('metadata:flush')
            throw new Error('metadata flush failed')
        })

        await expect(harness.flow.publish()).rejects.toThrow('pin failed')

        expect(harness.events).toEqual(['flush', 'pin', 'metadata:flush'])
        expect(harness.publication.publish).not.toHaveBeenCalled()
        expect(harness.publication.dispose).not.toHaveBeenCalled()
    })

    it('logs out by clearing native account keys and routing without remote work', async () => {
        const harness = createHarness(true)
        for (const key of Object.values(nativeOfficialAccountKeys)) harness.values.set(key, {})

        await harness.flow.logout()

        expect(harness.flushMetadata).toHaveBeenCalledOnce()
        expect(harness.appKv.remove).toHaveBeenCalledTimes(3)
        expect(harness.values.size).toBe(0)
        expect(harness.clearLegacyFallback).toHaveBeenCalledOnce()
        expect(harness.adapter.resetAccountAssociation).toHaveBeenCalledWith(null)
        expect(harness.setRouting).toHaveBeenCalledWith(null)
        expect(harness.adapter.pull).not.toHaveBeenCalled()
        expect(harness.adapter.pin).not.toHaveBeenCalled()
        await expect(harness.flow.restore()).rejects.toThrow(
            'Native official account login is required',
        )
    })

    it('clears bootstrap metadata mirrors and ledger binding across logout and relogin', async () => {
        const backend = new Map<string, unknown>([
            [nativeOfficialAccountKeys.association, {
                'officialAssociation:account-1': 'persisted-association',
            }],
            [nativeOfficialAccountKeys.assetLedger, {
                'officialPublishedAssets:account-1': JSON.stringify({
                    version: 1,
                    assets: { 'assets/old.png': 'remote/old.png' },
                    cold: {},
                }),
            }],
        ])
        const appKv: NativeAppKv = {
            get: vi.fn(async (key) => backend.get(key) ?? null),
            set: vi.fn(async (key, value) => void backend.set(key, value)),
            remove: vi.fn(async (key) => void backend.delete(key)),
        }
        const association = await createNativeAppKvStringStorage(
            appKv,
            nativeOfficialAccountKeys.association,
        )
        const assetLedger = await createNativeAppKvStringStorage(
            appKv,
            nativeOfficialAccountKeys.assetLedger,
        )
        let accountId: string | undefined = 'account-1'
        const ledger = createAccountScopedOfficialAssetLedger(
            assetLedger.storage,
            () => accountId,
        )
        expect(ledger.publishedAs('assets/old.png')).toBe('remote/old.png')
        const flow = createNativeOfficialAccountFlow({
            appKv,
            adapter: {
                pull: vi.fn(),
                pin: vi.fn(),
                resetAccountAssociation: vi.fn(),
            },
            initialCredential: { id: 'account-1', token: 'legacy-token', data: {} },
            flushPendingData: vi.fn(),
            getRevision: () => 1,
            restart: vi.fn(),
            setRouting: (credential) => {
                accountId = credential?.id
            },
            clearLegacyFallback: vi.fn(),
            flushMetadata: async () => {
                await association.flush()
                await assetLedger.flush()
            },
            resetMetadata: () => {
                association.reset()
                assetLedger.reset()
                ledger.reset()
            },
            resetAccountSession: vi.fn(),
        })

        await flow.logout()
        await flow.login({ id: 'account-1', token: 'new-token', data: {} })

        expect(association.storage.getItem('officialAssociation:account-1')).toBeNull()
        expect(ledger.publishedAs('assets/old.png')).toBeNull()
        expect(backend.has(nativeOfficialAccountKeys.association)).toBe(false)
        expect(backend.has(nativeOfficialAccountKeys.assetLedger)).toBe(false)
    })
})
