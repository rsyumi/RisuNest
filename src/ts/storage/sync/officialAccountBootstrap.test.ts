import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { RevisionConflictError } from '../persistentDataStore'
import type { AccountReadResult } from '../accountStorage'
import { fixtureDatabase } from '../tests/persistentDataFixtures'
import {
    initializeOfficialAccountBootstrap,
    publishOfficialRevisionIfChanged,
    type OfficialAccountBootstrapDependencies,
} from './officialAccountBootstrap'

function database(username: string): Database {
    const value = structuredClone(fixtureDatabase)
    value.username = username
    value.account = { token: 'synthetic-token' } as Database['account']
    return value
}

function makeHarness(input: {
    accountEnabled?: boolean
    syncRequested?: boolean
    remoteRead?: AccountReadResult
    pull?: OfficialAccountBootstrapDependencies['adapter']['pull']
    materialized?: Database
}) {
    const local = database('Local user')
    const markers = new Map<string, string>()
    if (input.accountEnabled) markers.set('accountst', 'able')
    if (input.syncRequested) markers.set('dosync', 'sync')
    const getMarker = vi.fn((key: string) => markers.get(key) ?? null)
    const events: string[] = []
    const publication = {
        publish: vi.fn(async () => { events.push('push') }),
        dispose: vi.fn(async () => { events.push('dispose') }),
    }
    const adapter = {
        pull: vi.fn(input.pull ?? (async () => ({ kind: 'missing' as const }))),
        pin: vi.fn(async () => {
            events.push('pin')
            return publication
        }),
    }
    const dependencies: OfficialAccountBootstrapDependencies = {
        isTauri: false,
        local: { database: local, revision: 1, profile: 'scalable-v3' },
        resolveWorkingSet: vi.fn(async (revision) => {
            events.push('resolve')
            return {
                database: structuredClone(input.materialized ?? local),
                revision,
                profile: 'scalable-v3' as const,
            }
        }),
        adapter,
        readRemoteDatabase: vi.fn(async (): Promise<AccountReadResult> =>
            input.remoteRead ?? { kind: 'missing' }),
        markers: {
            getItem: getMarker,
            setItem: (key, value) => {
                events.push(`marker:${key}`)
                markers.set(key, value)
            },
        },
        accountMode: { isAccount: input.accountEnabled ?? false },
        configurePublisher: vi.fn((publisher) => {
            events.push(publisher ? 'publisher:on' : 'publisher:off')
        }),
        assetReader: vi.fn(async () => new Uint8Array([7])),
        configureAssetReader: vi.fn((reader) => {
            events.push(reader ? 'asset:on' : 'asset:off')
        }),
        chooseExistingRemote: vi.fn(async (): Promise<'pull' | 'push'> => 'pull'),
        confirmInitialPush: vi.fn(async () => true),
        initializeProfile: vi.fn(() => { events.push('profile') }),
        installDatabase: vi.fn(() => { events.push('install') }),
        initializeWorkingSet: vi.fn(async () => { events.push('initialize') }),
        onRemoteError: vi.fn((error) => { events.push(`error:${(error as Error).message}`) }),
    }
    return { adapter, dependencies, events, getMarker, local, markers, publication }
}

describe('official account production bootstrap', () => {
    it('publishes a startup cold replacement only by pinning its committed revision', async () => {
        const publish = vi.fn(async () => undefined)
        const dispose = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose }))

        await publishOfficialRevisionIfChanged(true, { pin }, 4)
        await publishOfficialRevisionIfChanged(false, { pin }, 5)

        expect(pin).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledWith(4)
        expect(publish).toHaveBeenCalledOnce()
        expect(dispose).toHaveBeenCalledOnce()
    })

    it('preserves the publish error when publish and dispose both fail', async () => {
        const publishError = new Error('publish failed')
        const disposeError = new Error('dispose failed')
        const publication = {
            publish: vi.fn(async () => { throw publishError }),
            dispose: vi.fn(async () => { throw disposeError }),
        }

        await expect(publishOfficialRevisionIfChanged(
            true,
            { pin: vi.fn(async () => publication) },
            4,
        )).rejects.toBe(publishError)

        expect(publication.dispose).toHaveBeenCalledOnce()
    })

    it('reports dispose failure after a successful publish', async () => {
        const disposeError = new Error('dispose failed')
        const publication = {
            publish: vi.fn(async () => undefined),
            dispose: vi.fn(async () => { throw disposeError }),
        }

        await expect(publishOfficialRevisionIfChanged(
            true,
            { pin: vi.fn(async () => publication) },
            4,
        )).rejects.toBe(disposeError)

        expect(publication.publish).toHaveBeenCalledOnce()
    })

    it('initializes local data without account requests when account mode is disabled', async () => {
        const harness = makeHarness({})

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result).toMatchObject({ revision: 1, officialEnabled: false })
        expect(harness.dependencies.readRemoteDatabase).not.toHaveBeenCalled()
        expect(harness.adapter.pull).not.toHaveBeenCalled()
        expect(harness.adapter.pin).not.toHaveBeenCalled()
        expect(harness.events).toEqual([
            'publisher:off',
            'asset:off',
            'profile',
            'install',
            'initialize',
        ])
    })

    it('skips legacy official account bootstrap on Tauri while installing local SQLite data', async () => {
        const harness = makeHarness({ accountEnabled: true, syncRequested: true })
        harness.local.account = { token: 'synthetic-token', useSync: true } as Database['account']
        harness.dependencies.isTauri = true

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result).toMatchObject({ database: harness.local, revision: 1, officialEnabled: false })
        expect(harness.getMarker).not.toHaveBeenCalled()
        expect(harness.dependencies.readRemoteDatabase).not.toHaveBeenCalled()
        expect(harness.adapter.pull).not.toHaveBeenCalled()
        expect(harness.adapter.pin).not.toHaveBeenCalled()
        expect(harness.dependencies.installDatabase).toHaveBeenCalledWith(harness.local)
        expect(harness.dependencies.initializeWorkingSet).toHaveBeenCalledWith(harness.local)
        expect(harness.dependencies.accountMode.isAccount).toBe(false)
        expect(harness.events).toEqual([
            'publisher:off',
            'asset:off',
            'profile',
            'install',
            'initialize',
        ])
    })

    it('activates a fresh remote revision before installing the final working set', async () => {
        const remote = database('Remote user')
        const harness = makeHarness({
            accountEnabled: true,
            materialized: remote,
            pull: async () => {
                harness.events.push('pull')
                return { kind: 'activated', revision: 2 }
            },
        })

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result).toMatchObject({ database: remote, revision: 2, officialEnabled: true })
        expect(harness.events).toEqual([
            'publisher:off',
            'asset:off',
            'pull',
            'publisher:on',
            'asset:on',
            'resolve',
            'profile',
            'install',
            'initialize',
        ])
    })

    it('keeps the local working set when the pull preserves unpublished revisions', async () => {
        const harness = makeHarness({
            accountEnabled: true,
            pull: async () => ({ kind: 'kept-local', conflict: true }),
        })
        harness.dependencies.onPullSkipped = vi.fn((input) => {
            harness.events.push(`skip:${input.conflict}`)
        })

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result).toMatchObject({ database: harness.local, revision: 1, officialEnabled: true })
        expect(harness.dependencies.resolveWorkingSet).not.toHaveBeenCalled()
        expect(harness.adapter.pin).not.toHaveBeenCalled()
        expect(harness.dependencies.onPullSkipped).toHaveBeenCalledWith({ conflict: true })
        expect(harness.events).toEqual([
            'publisher:off',
            'asset:off',
            'skip:true',
            'publisher:on',
            'asset:on',
            'profile',
            'install',
            'initialize',
        ])
    })

    it('publishes the local revision explicitly when a new account has no remote database', async () => {
        const harness = makeHarness({ syncRequested: true })

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result.officialEnabled).toBe(true)
        expect(harness.adapter.pin).toHaveBeenCalledWith(1)
        expect(harness.publication.publish).toHaveBeenCalledOnce()
        expect(harness.publication.dispose).toHaveBeenCalledOnce()
        expect(harness.markers.get('accountst')).toBe('able')
        expect(harness.markers.get('dosync')).toBe('sync')
        expect(harness.dependencies.accountMode.isAccount).toBe(true)
        expect(harness.events.indexOf('push')).toBeLessThan(harness.events.indexOf('marker:accountst'))
    })

    it('publishes the local revision explicitly when an existing account pull is missing', async () => {
        const harness = makeHarness({ accountEnabled: true })

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result).toMatchObject({ revision: 1, officialEnabled: true })
        expect(harness.adapter.pull).toHaveBeenCalledOnce()
        expect(harness.adapter.pin).toHaveBeenCalledWith(1)
        expect(harness.publication.publish).toHaveBeenCalledOnce()
        expect(harness.dependencies.confirmInitialPush).toHaveBeenCalledOnce()
    })

    it('remembers a rejected recovery upload and does not prompt again on restart', async () => {
        const harness = makeHarness({ accountEnabled: true })
        harness.dependencies.confirmInitialPush = vi.fn(async () => false)

        await expect(initializeOfficialAccountBootstrap(harness.dependencies)).resolves.toMatchObject({
            revision: 1,
            officialEnabled: false,
        })
        expect(harness.markers.get('accountst')).toBe('able')
        expect(harness.markers.get('dosync')).toBe('avoid')

        await expect(initializeOfficialAccountBootstrap(harness.dependencies)).resolves.toMatchObject({
            revision: 1,
            officialEnabled: false,
        })
        expect(harness.adapter.pull).toHaveBeenCalledOnce()
        expect(harness.dependencies.confirmInitialPush).toHaveBeenCalledOnce()
    })

    it('propagates one pull CAS conflict without markers or bootstrap continuation', async () => {
        const harness = makeHarness({
            syncRequested: true,
            remoteRead: { kind: 'value', bytes: Uint8Array.of(1) },
            pull: async () => { throw new RevisionConflictError(1, 2) },
        })
        const continuePlugins = vi.fn()
        const bootstrap = async () => {
            await initializeOfficialAccountBootstrap(harness.dependencies)
            continuePlugins()
        }

        await expect(bootstrap()).rejects.toBeInstanceOf(RevisionConflictError)

        expect(harness.adapter.pull).toHaveBeenCalledOnce()
        expect(harness.markers.has('accountst')).toBe(false)
        expect(harness.dependencies.installDatabase).not.toHaveBeenCalled()
        expect(harness.dependencies.initializeWorkingSet).not.toHaveBeenCalled()
        expect(continuePlugins).not.toHaveBeenCalled()
    })

    it('preserves local authority and markers when a remote transition fails', async () => {
        const harness = makeHarness({
            syncRequested: true,
            remoteRead: { kind: 'value', bytes: Uint8Array.of(1) },
            pull: async () => { throw new Error('remote unavailable') },
        })

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result).toMatchObject({ database: harness.local, revision: 1, officialEnabled: false })
        expect(harness.markers.has('accountst')).toBe(false)
        expect(harness.dependencies.accountMode.isAccount).toBe(false)
        expect(harness.dependencies.onRemoteError).toHaveBeenCalledOnce()
        expect(harness.events.slice(-2)).toEqual(['install', 'initialize'])
    })

    it('keeps an existing marker but disables current-session account routes after pull failure', async () => {
        const harness = makeHarness({
            accountEnabled: true,
            pull: async () => { throw new Error('account offline') },
        })

        const result = await initializeOfficialAccountBootstrap(harness.dependencies)

        expect(result.officialEnabled).toBe(false)
        expect(harness.markers.get('accountst')).toBe('able')
        expect(harness.dependencies.accountMode.isAccount).toBe(false)
        expect(harness.events).toEqual([
            'publisher:off',
            'asset:off',
            'error:account offline',
            'profile',
            'install',
            'initialize',
        ])
    })
})
