import { describe, expect, it, vi } from 'vitest'

import {
    createPeerCloneFacade,
    initialPeerCloneState,
    pairingUriForQr,
    parsePeerCloneUri,
    reducePeerCloneState,
} from './peerClone'
import type { PeerCloneInvoke } from './peerClone'

const pairingUri = 'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret'

describe('parsePeerCloneUri', () => {
    it('parses the strict v1 LAN pairing URI and preserves the fragment claim separately', () => {
        expect(parsePeerCloneUri(pairingUri)).toEqual({
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            claim: 'claim-secret',
        })
        expect(pairingUriForQr(pairingUri)).toBe(pairingUri)
    })

    it.each([
        'https://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/nope?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=https%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Fuser%3Apass%40192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123%23bad&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Fexample.com%3A43123&session=invalid&manifest=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#claim=',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&endpoint=http%3A%2F%2F192.168.1.5%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&extra=x#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123%2Fv1%2Fsessions%2Fother&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F999.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F127.0.0.1%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F224.0.0.1%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2Flocalhost%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
        'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A0&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=claim-secret',
    ])('rejects malformed or unsafe pairing URI %s', (uri) => {
        expect(() => parsePeerCloneUri(uri)).toThrow()
    })

    it('rejects overlong values and malformed fragment encoding with the standard parser error', () => {
        expect(() => parsePeerCloneUri(`${pairingUri}${'x'.repeat(8192)}`)).toThrow('Invalid peer clone pairing URI')
        expect(() => parsePeerCloneUri(pairingUri.replace('claim-secret', '%E0%A4%A'))).toThrow('Invalid peer clone pairing URI')
    })
})

describe('PeerClone facade', () => {
    it('keeps the claim out of join and download, and sends it only in the claim command body', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string, ..._args: unknown[]): Promise<T> => (command === 'peer_clone_capabilities'
            ? {
                desktop: true,
                sourceReady: true,
                atomicActivationReady: true,
                largeFixturePassed: true,
                productionEnabled: true,
            }
            : undefined) as T)
        const facade = createPeerCloneFacade({ platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke })

        facade.join(pairingUri)
        await expect(facade.download()).rejects.toThrow('confirmation')
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(invoke).toHaveBeenCalledWith('peer_clone_claim_client', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            claim: 'claim-secret',
        })
        expect(invoke).toHaveBeenCalledWith('peer_clone_download', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        })
        expect(JSON.stringify(invoke.mock.calls.filter(([command]) => command !== 'peer_clone_claim_client')))
            .not.toContain('claim-secret')
    })

    it('keeps public source and target operations closed until native production gates pass', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(): Promise<T> => ({
            desktop: true,
            sourceReady: false,
            atomicActivationReady: false,
            largeFixturePassed: false,
            productionEnabled: false,
        } as T))
        const facade = createPeerCloneFacade({ platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await expect(facade.start('source-session')).rejects.toThrow('not enabled')
        await expect(facade.download()).rejects.toThrow('not enabled')
        await expect(facade.resume()).rejects.toThrow('not enabled')

        expect(invoke.mock.calls).toEqual([
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
        ])
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
})
