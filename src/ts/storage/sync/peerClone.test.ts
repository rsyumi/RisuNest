import { describe, expect, it, vi } from 'vitest'

import {
    createPeerCloneFacade,
    initialPeerCloneState,
    pairingUriForQr,
    parsePeerCloneUri,
    reducePeerCloneState,
} from './peerClone'
import type { PeerCloneInvoke, PeerCloneReplacementRuntime } from './peerClone'

const claim = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
const pairingUri = `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`
const quickTunnelPairingUri = `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fexample-id.trycloudflare.com&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`
const namedTunnelPairingUri = `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`

describe('parsePeerCloneUri', () => {
    it('parses the strict v1 LAN pairing URI and preserves the fragment claim separately', () => {
        expect(parsePeerCloneUri(pairingUri)).toEqual({
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            claim,
        })
        expect(pairingUriForQr(pairingUri)).toBe(pairingUri)
    })

    it.each([
        [quickTunnelPairingUri, 'https://example-id.trycloudflare.com/'],
        [namedTunnelPairingUri, 'https://sync.example.com/'],
    ])('accepts a bare public HTTPS tunnel endpoint', (uri, endpoint) => {
        expect(parsePeerCloneUri(uri)).toEqual({
            endpoint,
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            claim,
        })
    })

    it.each([
        `https://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/nope?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Fuser%3Apass%40192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123%23bad&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Fexample.com%3A43123&session=invalid&manifest=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#claim=',
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&endpoint=http%3A%2F%2F192.168.1.5%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&extra=x#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123%2Fv1%2Fsessions%2Fother&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123%2Fv1%2Fsessions%2F123e4567-e89b-12d3-a456-426614174000&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F999.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F127.0.0.1%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F224.0.0.1%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Flocalhost%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F8.8.8.8%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Fexample.com%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A0&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com%3A8443&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com%3A443&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com%2Fclone&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com%2F.&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com%2Fa%2F..&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fsync.example.com%2F%252e&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Fuser%40sync.example.com&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2Flocalhost&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
        `risuailocal://peer-clone/v1?endpoint=https%3A%2F%2F127.0.0.1&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=${claim}`,
    ])('rejects malformed or unsafe pairing URI %s', (uri) => {
        expect(() => parsePeerCloneUri(uri)).toThrow()
    })

    it('rejects overlong values and malformed fragment encoding with the standard parser error', () => {
        expect(() => parsePeerCloneUri(`${pairingUri}${'x'.repeat(8192)}`)).toThrow('Invalid peer clone pairing URI')
        expect(() => parsePeerCloneUri(pairingUri.replace(claim, '%E0%A4%A'))).toThrow('Invalid peer clone pairing URI')
        expect(() => parsePeerCloneUri(pairingUri.replace(claim, 'A'.repeat(64)))).toThrow('Invalid peer clone pairing URI')
    })
})

describe('PeerClone facade', () => {
    it('keeps the claim out of join and download, and sends it only in the claim command body', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string, ..._args: unknown[]): Promise<T> => (command === 'peer_clone_capabilities'
            ? {
                desktop: true,
                sourceReady: true,
                atomicActivationReady: true,
                losslessBackupReady: true,
                httpTransportReady: true,
                largeFixturePassed: true,
                productionEnabled: true,
            }
            : undefined) as T)
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        facade.join(pairingUri)
        await expect(facade.download()).rejects.toThrow('confirmation')
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(invoke).toHaveBeenCalledWith('peer_clone_claim_client', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            claim,
        })
        expect(invoke).toHaveBeenCalledWith('peer_clone_download', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        })
        expect(JSON.stringify(invoke.mock.calls.filter(([command]) => command !== 'peer_clone_claim_client')))
            .not.toContain(claim)
    })

    it('keeps public source and target operations closed until native production gates pass', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(): Promise<T> => ({
            desktop: true,
            sourceReady: false,
            atomicActivationReady: false,
            losslessBackupReady: false,
            httpTransportReady: false,
            largeFixturePassed: false,
            productionEnabled: false,
        } as T))
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await expect(facade.prepare()).rejects.toThrow('not enabled')
        await expect(facade.start('source-session')).rejects.toThrow('not enabled')
        await expect(facade.download()).rejects.toThrow('not enabled')
        facade.confirmDestructiveReplace()
        await expect(facade.resume()).rejects.toThrow('not enabled')

        expect(invoke.mock.calls).toEqual([
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
        ])
    })

    it.each([
        { name: 'lossless backup', losslessBackupReady: false, httpTransportReady: true },
        { name: 'HTTP transport', losslessBackupReady: true, httpTransportReady: false },
    ])('does not trust productionEnabled when the $name gate is closed', async ({ name: _name, ...gate }) => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(): Promise<T> => ({
            desktop: true,
            sourceReady: true,
            atomicActivationReady: true,
            largeFixturePassed: true,
            productionEnabled: true,
            ...gate,
        } as T))
        const facade = createPeerCloneFacade({ platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await expect(facade.prepare()).rejects.toThrow('not enabled')
        await expect(facade.download()).rejects.toThrow('not enabled')
        expect(invoke.mock.calls).toEqual([
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
        ])
    })

    it('treats the large fixture result as release evidence instead of a runtime gate', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => (command === 'peer_clone_capabilities'
            ? {
                desktop: true,
                sourceReady: true,
                atomicActivationReady: true,
                losslessBackupReady: true,
                httpTransportReady: true,
                largeFixturePassed: false,
                productionEnabled: true,
            }
            : command === 'peer_clone_prepare'
                ? { phase: 'prepared', sessionId: 'source-session', devices: [] }
                : undefined) as T)
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await expect(facade.prepare()).resolves.toEqual({ phase: 'prepared', sessionId: 'source-session', devices: [] })
        await expect(facade.download()).resolves.toBeUndefined()
    })

    it('flushes source writes before native lossless preparation', async () => {
        const events: string[] = []
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_prepare') {
                events.push('prepare')
                return { phase: 'prepared', sessionId: 'source-session', devices: [] } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                async flushPendingData(reason: string) {
                    events.push(`flush:${reason}`)
                },
                capturePersistentMutationToken: vi.fn(),
                acquireDestructiveReplacementFence: vi.fn(),
            },
        })

        await facade.prepare()

        expect(events).toEqual(['flush:peer-clone-source-prepare', 'prepare'])
    })

    it('returns the pairing claim only from start and keeps later source status claim-free', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_start') {
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    manifestId: 'a'.repeat(64),
                    pairingUri,
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_status') {
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    manifestId: 'a'.repeat(64),
                    devices: [],
                } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        await expect(facade.start('source-session')).resolves.toMatchObject({ pairingUri })
        const status = await facade.sourceStatus()

        expect(status.pairingUri).toBeUndefined()
        expect(JSON.stringify(status)).not.toContain(claim)
    })

    it('starts Quick Tunnel through the one-shot tunnel command and exposes no persistent endpoint metadata', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_tunnel_start') {
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    manifestId: 'a'.repeat(64),
                    pairingUri: quickTunnelPairingUri,
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_tunnel_status') {
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        const started = await facade.startQuickTunnel('source-session')
        const status = await facade.tunnelStatus()

        expect(invoke).toHaveBeenCalledWith('peer_clone_tunnel_start', {
            sessionId: 'source-session',
            tunnel: { kind: 'quick' },
        })
        expect(started).toMatchObject({
            pairingUri: quickTunnelPairingUri,
            tunnel: { kind: 'quick', experimental: true, oneShot: true },
        })
        expect(status).toEqual({
            phase: 'running',
            sessionId: 'source-session',
            tunnel: { kind: 'quick', experimental: true, oneShot: true },
        })
        expect(JSON.stringify(status)).not.toContain('trycloudflare.com')
    })

    it('submits a Named Tunnel token once without retaining or reflecting it in state and errors', async () => {
        const token = 'named-tunnel-token-that-is-at-least-32-bytes'
        const probeUrl = 'https://sync.example.com/v1/sessions/id/tunnel-check/probe-secret'
        let fail = false
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_tunnel_start') {
                if (fail) throw new Error(`${token} ${probeUrl}`)
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    manifestId: 'a'.repeat(64),
                    pairingUri: namedTunnelPairingUri,
                    tunnel: { kind: 'named', experimental: false, oneShot: false },
                    devices: [],
                } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        await facade.startNamedTunnel('source-session', token, 'https://sync.example.com')
        expect(invoke).toHaveBeenCalledWith('peer_clone_tunnel_start', {
            sessionId: 'source-session',
            tunnel: {
                kind: 'named',
                token,
                expectedPublicBaseUrl: 'https://sync.example.com',
            },
        })
        expect(JSON.stringify(facade.getState())).not.toContain(token)
        expect(JSON.stringify(facade.getState())).not.toContain(probeUrl)

        fail = true
        const failed = facade.startNamedTunnel('source-session', token, 'https://sync.example.com')
        await expect(failed).rejects.toThrow('Named tunnel failed to start')
        await expect(failed).rejects.not.toThrow(token)
        await expect(failed).rejects.not.toThrow(probeUrl)
        expect(JSON.stringify(facade.getState())).not.toContain(token)
    })

    it('preserves only the native fixed-port recovery guidance for Named Tunnel', async () => {
        const message = 'Named Tunnel cannot bind loopback port 32145. Stop the app using that port, or use Quick Tunnel / Trusted LAN.'
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_tunnel_start') throw new Error(message)
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        await expect(
            facade.startNamedTunnel(
                'source-session',
                'named-tunnel-token-that-is-at-least-32-bytes',
                'https://sync.example.com',
            ),
        ).rejects.toThrow(message)
    })

    it('finalizes only after native download reaches the activation barrier', async () => {
        const events: string[] = []
        let finalized = false
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_claim_client') {
                events.push('claim')
                return undefined as T
            }
            if (command === 'peer_clone_download') {
                events.push('download')
                return undefined as T
            }
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                events.push('finalize')
                finalized = true
                return { revision: 42 } as T
            }
            if (command === 'peer_clone_release_target') {
                events.push('native-release')
                return undefined as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken(reason: string) {
                    events.push(`capture:${reason}`)
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence(token: { revision: number; mutationGeneration: number }) {
                    events.push(`acquire:${token.revision}:${token.mutationGeneration}`)
                    return {
                        async refreshCommittedWorkingSet(revision: number) {
                            events.push(`refresh:${revision}`)
                        },
                        release() {
                            events.push('release')
                        },
                    }
                },
            },
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await facade.download()
        expect(events).toEqual(['claim', 'download'])

        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(events).toEqual([
            'claim',
            'download',
            'capture:peer-clone-target-finalize',
            'acquire:41:7',
            'finalize',
            'refresh:42',
            'native-release',
            'release',
        ])
        expect(facade.getState().target).toMatchObject({
            phase: 'completed',
            completedBytes: 10,
            totalBytes: 10,
        })
    })

    it('releases the replacement fence when native finalize fails', async () => {
        const events: string[] = []
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') {
                events.push('finalize')
                throw new Error('revision conflict')
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    events.push('capture')
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    events.push('acquire')
                    return {
                        refreshCommittedWorkingSet: vi.fn(),
                        release() {
                            events.push('release')
                        },
                    }
                },
            },
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow('revision conflict')
        expect(events).toEqual(['capture', 'acquire', 'finalize', 'release'])
        expect(facade.getState().target.phase).toBe('failed')
    })

    it.each(['capture', 'acquire'] as const)('clears a failed %s handshake so finalization can retry', async (failure) => {
        let attempt = 0
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') return { revision: 42 } as T
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    if (failure === 'capture' && attempt++ === 0) throw new Error('capture failed')
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    if (failure === 'acquire' && attempt++ === 0) throw new Error('acquire failed')
                    return {
                        refreshCommittedWorkingSet: vi.fn(),
                        release: vi.fn(),
                    }
                },
            },
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow(`${failure} failed`)
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
    })

    it('retains the committed revision and fence until renderer refresh succeeds', async () => {
        const events: string[] = []
        let finalized = false
        let refreshAttempt = 0
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                events.push('finalize')
                finalized = true
                return { revision: 42 } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    events.push('capture')
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    events.push('acquire')
                    return {
                        async refreshCommittedWorkingSet(revision: number) {
                            events.push(`refresh:${revision}`)
                            if (refreshAttempt++ === 0) throw new Error('refresh failed')
                        },
                        release() {
                            events.push('release')
                        },
                    }
                },
            },
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow('refresh failed')
        expect(facade.getState().target.phase).not.toBe('failed')
        expect(events).toEqual(['capture', 'acquire', 'finalize', 'refresh:42'])

        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(events).toEqual(['capture', 'acquire', 'finalize', 'refresh:42', 'refresh:42', 'release'])
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_release_target')).toHaveLength(1)
    })

    it('retains the completed native target until release succeeds after one renderer refresh', async () => {
        let finalized = false
        let releaseAttempt = 0
        const refresh = vi.fn(async (_revision: number) => undefined)
        const fenceRelease = vi.fn()
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                finalized = true
                return { revision: 42 } as T
            }
            if (command === 'peer_clone_release_target' && releaseAttempt++ === 0) {
                throw new Error('native release failed')
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(refresh, fenceRelease),
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow('native release failed')
        expect(facade.getState().target.phase).not.toBe('completed')
        expect(fenceRelease).not.toHaveBeenCalled()
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })

        expect(refresh).toHaveBeenCalledTimes(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_release_target')).toHaveLength(2)
        expect(fenceRelease).toHaveBeenCalledTimes(1)
    })

    it('captures target identity before the asynchronous finalize handshake', async () => {
        let releaseCapture: (() => void) | undefined
        const captureBlocked = new Promise<void>((resolve) => {
            releaseCapture = resolve
        })
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') return { revision: 42 } as T
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    await captureBlocked
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    return {
                        refreshCommittedWorkingSet: vi.fn(),
                        release: vi.fn(),
                    }
                },
            },
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        const finalizing = facade.targetStatus()
        await Promise.resolve()
        expect(() => facade.join(
            pairingUri.replace('123e4567-e89b-12d3-a456-426614174000', '223e4567-e89b-42d3-a456-426614174000'),
        )).toThrow('finalization is still active')
        releaseCapture?.()

        await finalizing
        expect(invoke).toHaveBeenCalledWith('peer_clone_finalize', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        })
    })

    it('does not finalize while native transfer is still downloading, and resume does not reclaim', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_target_status') {
                return { phase: 'downloading', completedBytes: 8, totalBytes: 10 } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                capturePersistentMutationToken: vi.fn(),
                acquireDestructiveReplacementFence: vi.fn(),
            },
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await facade.resume()
        await facade.targetStatus()

        expect(invoke).toHaveBeenCalledWith('peer_clone_resume', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        })
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_claim_client')).toBe(false)
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_finalize')).toBe(false)
    })

    it('locks one target identity before the first download await and uses one captured claim', async () => {
        let releaseCapabilities: (() => void) | undefined
        const capabilitiesBlocked = new Promise<void>((resolve) => {
            releaseCapabilities = resolve
        })
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') {
                await capabilitiesBlocked
                return productionCapabilities() as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        const downloading = facade.download()
        await Promise.resolve()
        expect(() => facade.join(
            pairingUri.replace('123e4567-e89b-12d3-a456-426614174000', '223e4567-e89b-42d3-a456-426614174000'),
        )).toThrow('already owned')
        releaseCapabilities?.()
        await downloading

        expect(invoke).toHaveBeenCalledWith('peer_clone_claim_client', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
            claim,
        })
        expect(invoke).toHaveBeenCalledWith('peer_clone_download', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
        })
    })

    it.each(['capabilities', 'claim', 'download'] as const)(
        'keeps the same target retryable when the initial %s step fails',
        async (failure) => {
            let failed = false
            const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
                const failingCommand = failure === 'capabilities'
                    ? 'peer_clone_capabilities'
                    : failure === 'claim'
                        ? 'peer_clone_claim_client'
                        : 'peer_clone_download'
                if (command === failingCommand && !failed) {
                    failed = true
                    throw new Error(`${failure} failed`)
                }
                if (command === 'peer_clone_capabilities') return productionCapabilities() as T
                return undefined as T
            })
            const facade = createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: replacementRuntime(),
            })
            facade.join(pairingUri)
            facade.confirmDestructiveReplace()

            await expect(facade.download()).rejects.toThrow(`${failure} failed`)
            expect(() => facade.join(pairingUri.replace(
                '123e4567-e89b-12d3-a456-426614174000',
                '223e4567-e89b-42d3-a456-426614174000',
            ))).toThrow('already owned')

            if (failure === 'download') {
                expect(facade.getState().target.phase).toBe('failed')
                await facade.resume()
            } else {
                expect(facade.getState().target.phase).toBe('joined')
                facade.confirmDestructiveReplace()
                await facade.download()
            }

            expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_claim_client')).toHaveLength(
                failure === 'capabilities' ? 1 : failure === 'claim' ? 2 : 1,
            )
            expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_download')).toHaveLength(1)
            expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_resume')).toHaveLength(
                failure === 'download' ? 1 : 0,
            )
            expect(facade.getState().target.phase).toBe('downloading')
        },
    )

    it.each(['failed', 'cancelled'] as const)(
        'accepts a fresh pairing only for the same %s transfer identity',
        async (phase) => {
            let failDownload = phase === 'failed'
            const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
                if (command === 'peer_clone_capabilities') return productionCapabilities() as T
                if (command === 'peer_clone_download' && failDownload) {
                    failDownload = false
                    throw new Error('source restarted')
                }
                return undefined as T
            })
            const facade = createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: replacementRuntime(),
            })
            facade.join(pairingUri)
            facade.confirmDestructiveReplace()
            if (phase === 'failed') {
                await expect(facade.download()).rejects.toThrow('source restarted')
            } else {
                await facade.download()
                await facade.cancel()
            }

            const repairedPairing = pairingUri
                .replace('192.168.1.4%3A43123', '192.168.1.5%3A43124')
                .replace(`#claim=${claim}`, `#claim=${'c'.repeat(64)}`)
            expect(() => facade.join(repairedPairing.replace(
                '123e4567-e89b-12d3-a456-426614174000',
                '223e4567-e89b-42d3-a456-426614174000',
            ))).toThrow('already owned')

            expect(facade.join(repairedPairing).target).toMatchObject({
                phase: 'joined',
                pairing: {
                    endpoint: 'http://192.168.1.5:43124/',
                    sessionId: '123e4567-e89b-12d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    claim: 'c'.repeat(64),
                },
            })
            facade.confirmDestructiveReplace()
            await facade.download()

            expect(invoke).toHaveBeenLastCalledWith('peer_clone_download', {
                endpoint: 'http://192.168.1.5:43124/',
                sessionId: '123e4567-e89b-12d3-a456-426614174000',
                manifestId: 'a'.repeat(64),
            })
            expect(invoke).toHaveBeenCalledWith('peer_clone_claim_client', {
                endpoint: 'http://192.168.1.5:43124/',
                sessionId: '123e4567-e89b-12d3-a456-426614174000',
                manifestId: 'a'.repeat(64),
                claim: 'c'.repeat(64),
            })
        },
    )

    it('allows a different target only after committed refresh and native release complete', async () => {
        const events: string[] = []
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') return { revision: 42 } as T
            if (command === 'peer_clone_release_target') events.push(`native-release:${String(args?.sessionId)}`)
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(
                async () => { events.push('refresh') },
                () => { events.push('fence-release') },
            ),
        })
        const other = pairingUri.replace(
            '123e4567-e89b-12d3-a456-426614174000',
            '223e4567-e89b-42d3-a456-426614174000',
        )
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await facade.targetStatus()
        expect(events).toEqual([
            'refresh',
            'native-release:123e4567-e89b-12d3-a456-426614174000',
            'fence-release',
        ])
        expect(facade.join(other).target).toMatchObject({
            phase: 'joined',
            pairing: { sessionId: '223e4567-e89b-42d3-a456-426614174000' },
        })
    })

    it('discards a stale target status response after a different unowned pairing joins', async () => {
        let resolveStatus: ((value: unknown) => void) | undefined
        const status = new Promise((resolve) => {
            resolveStatus = resolve
        })
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') return await status as T
            if (command === 'peer_clone_finalize') throw new Error('stale response finalized')
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        const polling = facade.targetStatus()
        const other = pairingUri.replace(
            '123e4567-e89b-12d3-a456-426614174000',
            '223e4567-e89b-42d3-a456-426614174000',
        )
        facade.join(other)
        resolveStatus?.({ phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 })
        await polling

        expect(facade.getState().target).toMatchObject({
            phase: 'joined',
            pairing: { sessionId: '223e4567-e89b-42d3-a456-426614174000' },
        })
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_finalize')).toBe(false)
    })

    it('retains a bounded native post-commit warning after refreshing the committed revision', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') {
                return { revision: 42, warning: 'activation ledger cleanup failed' } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(facade.getWarning()).toBe('activation ledger cleanup failed')
    })

    it('reports web and Android as explicitly unsupported without invoking native commands', async () => {
        const invoke = vi.fn()
        expect(createPeerCloneFacade({ platform: 'web', invoke }).status()).toEqual({ kind: 'unsupported', platform: 'web' })
        expect(createPeerCloneFacade({ platform: 'android', invoke }).status()).toEqual({ kind: 'unsupported', platform: 'android' })
        expect(invoke).not.toHaveBeenCalled()
    })

    it('models source controls and resumable target progress as pure transitions', () => {
        let state = initialPeerCloneState
        state = reducePeerCloneState(state, { type: 'source-prepared', sessionId: 'source-session' })
        state = reducePeerCloneState(state, { type: 'source-started' })
        state = reducePeerCloneState(state, { type: 'source-stopped' })
        state = reducePeerCloneState(state, { type: 'source-revoked', deviceId: 'device-a' })
        state = reducePeerCloneState(state, { type: 'target-joined', pairing: parsePeerCloneUri(pairingUri) })
        state = reducePeerCloneState(state, { type: 'target-confirmed' })
        state = reducePeerCloneState(state, { type: 'target-progress', completedBytes: 8, totalBytes: 10 })
        state = reducePeerCloneState(state, { type: 'target-cancelled' })
        state = reducePeerCloneState(state, { type: 'target-resumed' })

        expect(state).toMatchObject({
            source: { phase: 'stopped', sessionId: 'source-session', revokedDeviceIds: ['device-a'] },
            target: { phase: 'downloading', destructiveConfirmed: true, completedBytes: 8, totalBytes: 10 },
        })
    })

    it('polls target progress without forwarding the one-time claim', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => ({
            phase: command === 'peer_clone_target_status' ? 'downloading' : undefined,
            completedBytes: 8,
            totalBytes: 10,
        } as T))
        const facade = createPeerCloneFacade({ platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await facade.targetStatus()

        expect(facade.getState().target).toMatchObject({
            phase: 'downloading',
            completedBytes: 8,
            totalBytes: 10,
        })
        expect(JSON.stringify(invoke.mock.calls)).not.toContain(claim)
    })
})

function productionCapabilities() {
    return {
        desktop: true,
        sourceReady: true,
        atomicActivationReady: true,
        losslessBackupReady: true,
        httpTransportReady: true,
        largeFixturePassed: false,
        productionEnabled: true,
    }
}

function replacementRuntime(
    refreshCommittedWorkingSet: (revision: number) => Promise<void> = vi.fn(async (_revision: number) => undefined),
    release: () => void = vi.fn(),
): PeerCloneReplacementRuntime {
    return {
        flushPendingData: vi.fn(async () => undefined),
        capturePersistentMutationToken: vi.fn(async () => ({ revision: 1, mutationGeneration: 0 })),
        acquireDestructiveReplacementFence: vi.fn(async () => ({
            refreshCommittedWorkingSet,
            release,
        })),
    }
}
