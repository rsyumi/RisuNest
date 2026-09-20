import { describe, expect, it } from 'vitest'
import {
    buildPrepareConnectionRequest,
    defaultExternalCapturePolicy,
    mergeExternalHistoryItems,
    restorableExternalHistoryItems,
    externalConflictActions,
} from './connection'
import { buildConnectionConfig, buildProviderSecret, externalProviderDefinitions } from './providerRegistry'

describe('external storage connection request', () => {
    it('keeps a capture policy off synchronization connections and requires one for a backup', () => {
        expect(() => buildPrepareConnectionRequest({
            providerId: 'google_drive',
            values: { folderId: 'folder', projectId: 'project', clientId: 'client' },
            platform: 'windows', mode: 'create', purpose: 'sync',
            capturePolicy: { hypa: true, localPlugins: false, localSettings: false },
            acknowledgements: [],
        })).toThrow('capture policy')
        expect(() => buildPrepareConnectionRequest({
            providerId: 'gitlab_packages',
            values: { endpoint: 'https://gitlab.example', accountId: 'user', profile: 'selfManaged', projectId: '1', packageName: 'risunest' },
            platform: 'windows', mode: 'create', purpose: 'backup',
            acknowledgements: [],
        })).toThrow('capture policy')
    })

    it('leaves publication strategy selection to the native connection', () => {
        const request = buildPrepareConnectionRequest({
            providerId: 'google_drive', values: {}, platform: 'android', mode: 'existing',
            purpose: 'sync', capturePolicy: defaultExternalCapturePolicy('sync'),
            acknowledgements: [],
        })
        expect(request).not.toHaveProperty('publicationStrategy')
        expect(request.acknowledgements).toEqual([])
    })

    it('constructs the authoritative connection config shape', () => {
        const request = buildPrepareConnectionRequest({
            providerId: 'gitlab_packages',
            values: { endpoint: 'https://gitlab.example', accountId: 'user', profile: 'selfManaged', projectId: '1', packageName: 'risunest' },
            platform: 'windows', mode: 'create', purpose: 'backup',
            capturePolicy: defaultExternalCapturePolicy('backup'), acknowledgements: [],
        })
        expect(request.config).toEqual({
            provider: 'gitlab_packages', profile: 'selfManaged', endpoint: 'https://gitlab.example', accountId: '',
            location: { projectId: '1', packageName: 'risunest' },
        })
    })

    it.each(externalProviderDefinitions.filter(provider => provider.id !== 'webdav'))(
        'derives $id identity natively without a renderer account label',
        provider => {
            expect(provider.fields.some(field => field.key === 'accountId')).toBe(false)
            expect(buildConnectionConfig(provider.id, {}, 'windows').accountId).toBe('')
            expect(buildConnectionConfig(provider.id, { accountId: 'untrusted-label' }, 'windows').accountId).toBe('')
        },
    )

    it('retains the WebDAV username required for authentication', () => {
        expect(buildConnectionConfig('webdav', { accountId: ' dav-user ' }, 'windows').accountId).toBe('dav-user')
    })

    it('sends a GitLab access token without a token-kind negotiation field', () => {
        expect(buildProviderSecret('gitlab_packages', { token: 'synthetic-access-token' }))
            .toEqual({ kind: 'gitlab', token: 'synthetic-access-token' })
    })

    it('puts the Android Google Web client and exact HTTPS callback in non-secret config', () => {
        const request = buildPrepareConnectionRequest({
            providerId: 'google_drive',
            values: {
                folderId: 'folder', space: 'drive', projectId: 'project',
                clientId: 'web-client.apps.googleusercontent.com',
                oauthRedirectUri: 'https://update.rsyumi.workers.dev/oauth/google-drive-callback',
                clientSecret: 'must-stay-transient',
            },
            platform: 'android', mode: 'create', purpose: 'backup',
            capturePolicy: defaultExternalCapturePolicy('backup'),
            acknowledgements: [],
        })

        expect(request.config.location.oauthRedirectUri).toBe(
            'https://update.rsyumi.workers.dev/oauth/google-drive-callback',
        )
        expect(request.config.oauthProfile?.platformClientIds).toEqual({
            android: 'web-client.apps.googleusercontent.com',
        })
        expect(JSON.stringify(request)).not.toContain('must-stay-transient')
    })

    it('does not place provider secrets in the preparation DTO', () => {
        const request = buildPrepareConnectionRequest({
            providerId: 'webdav',
            values: {
                endpoint: 'https://dav.example', accountId: 'user', root: 'RisuNest',
                password: 'must-not-be-in-preparation',
            },
            platform: 'windows', mode: 'create', purpose: 'backup',
            capturePolicy: defaultExternalCapturePolicy('backup'), acknowledgements: [],
        })
        expect(JSON.stringify(request)).not.toContain('must-not-be-in-preparation')
    })

    it('serializes MYBOX expiry as decimal milliseconds', () => {
        expect(buildProviderSecret('mybox', { pat: 'token', expiresAtMs: '2030-01-02T03:04' }))
            .toMatchObject({ kind: 'mybox', expiresAtMs: expect.stringMatching(/^\d+$/) })
    })

    it('deduplicates overlapping history pages by snapshot identifier', () => {
        const first = {
            id: 'snapshot-1', kind: 'snapshot' as const, createdAtMs: '1' as const,
            logicalRevision: '1' as const, pinned: false, complete: true, verified: true,
            includedSections: [], sameDevice: false,
        }
        const updated = { ...first, pinned: true }
        const second = { ...first, id: 'snapshot-2', logicalRevision: '2' as const }

        expect(mergeExternalHistoryItems([first], [updated, second])).toEqual([updated, second])
    })

    it('puts the newest entry first because pages arrive in object order', () => {
        const base = {
            kind: 'recovery-candidate' as const, logicalRevision: '1' as const,
            pinned: false, complete: true, verified: true,
            includedSections: [], sameDevice: false,
        }

        expect(mergeExternalHistoryItems(
            [{ ...base, id: 'aaa', createdAtMs: '1000' as const }],
            [
                { ...base, id: 'zzz', createdAtMs: '3000' as const },
                { ...base, id: 'mmm', createdAtMs: '2000' as const },
            ],
        ).map(item => item.id)).toEqual(['zzz', 'mmm', 'aaa'])
    })

    it('keeps separate point rows and hides their weaker raw recovery candidate', () => {
        const base = {
            createdAtMs: '1' as const, logicalRevision: '1' as const,
            pinned: false, complete: true, verified: true,
            includedSections: [], sameDevice: false,
        }
        const first = { ...base, id: 'point-a', snapshotId: 'snapshot', kind: 'backup-point' as const }
        const second = { ...base, id: 'point-b', snapshotId: 'snapshot', kind: 'backup-point' as const }
        const recovery = { ...base, id: 'snapshot', snapshotId: 'snapshot', kind: 'recovery-candidate' as const }
        expect(mergeExternalHistoryItems([first], [second, recovery]).map(item => item.id))
            .toEqual(['point-a', 'point-b'])
    })

    it('keeps only the entries a restore can read back', () => {
        const base = {
            kind: 'recovery-candidate' as const, createdAtMs: '1' as const,
            logicalRevision: '1' as const, pinned: false,
            includedSections: [], sameDevice: false,
        }

        expect(restorableExternalHistoryItems([
            { ...base, id: 'partial', complete: false, verified: true },
            { ...base, id: 'unverified', complete: true, verified: false },
            { ...base, id: 'usable', complete: true, verified: true },
        ]).map(item => item.id)).toEqual(['usable'])
    })

    it('preserves pinned conflict metadata when a later root page repeats a snapshot', () => {
        const conflict = {
            id: 'snapshot-1', kind: 'conflict' as const, createdAtMs: '1' as const,
            logicalRevision: '1' as const, pinned: true, complete: false, verified: false,
            includedSections: [], sameDevice: false,
        }
        const verifiedRoot = {
            ...conflict,
            kind: 'recovery-candidate' as const,
            pinned: false,
            complete: true,
            verified: true,
        }

        expect(mergeExternalHistoryItems([conflict], [verifiedRoot])).toEqual([{
            ...verifiedRoot,
            kind: 'conflict',
            pinned: true,
        }])
    })

    it('gates conflict resolution on durable remote confirmation and side availability', () => {
        const unconfirmed = {
            id: 'conflict-1', connectionId: 'connection', detectedAtMs: '1' as const,
            localRevision: '8' as const, remoteRevision: '7' as const,
            localAvailable: true, remoteAvailable: true,
            remotePointConfirmed: false, resolved: false,
        }

        expect(externalConflictActions(unconfirmed)).toEqual(['retry-sync'])
        expect(externalConflictActions({
            ...unconfirmed,
            remotePointConfirmed: true,
        })).toEqual(['keep-local', 'use-remote'])
        expect(externalConflictActions({
            ...unconfirmed,
            remotePointConfirmed: true,
            localAvailable: false,
        })).toEqual(['use-remote'])
        expect(externalConflictActions({
            ...unconfirmed,
            resolved: true,
        })).toEqual([])
    })
})
