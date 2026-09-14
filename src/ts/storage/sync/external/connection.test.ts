import { describe, expect, it } from 'vitest'
import {
    BACKUP_ONLY_ACKNOWLEDGEMENT,
    SEQUENTIAL_ACKNOWLEDGEMENT,
    buildPrepareConnectionRequest,
    defaultExternalStorageScope,
    mergeExternalHistoryItems,
    externalConflictActions,
} from './connection'
import { buildProviderSecret } from './providerRegistry'

describe('external storage connection request', () => {
    it('keeps device sections out of synchronization repositories', () => {
        expect(() => buildPrepareConnectionRequest({
            providerId: 'google_drive',
            values: { folderId: 'folder', projectId: 'project', clientId: 'client' },
            platform: 'windows', mode: 'create', purpose: 'sync', strategy: 'sequential',
            scope: { ...defaultExternalStorageScope('sync'), deviceSettings: true },
            acknowledgements: [SEQUENTIAL_ACKNOWLEDGEMENT],
        })).toThrow('device-only')
    })

    it('requires the sequential-use limitation acknowledgement', () => {
        expect(() => buildPrepareConnectionRequest({
            providerId: 'google_drive', values: {}, platform: 'android', mode: 'existing',
            purpose: 'sync', strategy: 'sequential', scope: defaultExternalStorageScope('sync'),
            acknowledgements: [],
        })).toThrow(SEQUENTIAL_ACKNOWLEDGEMENT)
    })

    it('constructs the authoritative connection config shape', () => {
        const request = buildPrepareConnectionRequest({
            providerId: 'gitlab_packages',
            values: { endpoint: 'https://gitlab.example', accountId: 'user', profile: 'selfManaged', projectId: '1', packageName: 'risunest' },
            platform: 'windows', mode: 'create', purpose: 'backup', strategy: 'backup-only',
            scope: defaultExternalStorageScope('backup'), acknowledgements: [BACKUP_ONLY_ACKNOWLEDGEMENT],
        })
        expect(request.config).toEqual({
            provider: 'gitlab_packages', profile: 'selfManaged', endpoint: 'https://gitlab.example', accountId: 'user',
            location: { projectId: '1', packageName: 'risunest' },
        })
    })

    it('puts the Android Google Web client and exact HTTPS callback in non-secret config', () => {
        const request = buildPrepareConnectionRequest({
            providerId: 'google_drive',
            values: {
                folderId: 'folder', space: 'drive', projectId: 'project',
                clientId: 'web-client.apps.googleusercontent.com',
                oauthRedirectUri: 'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html',
                clientSecret: 'must-stay-transient',
            },
            platform: 'android', mode: 'create', purpose: 'backup', strategy: 'backup-only',
            scope: defaultExternalStorageScope('backup'),
            acknowledgements: [BACKUP_ONLY_ACKNOWLEDGEMENT],
        })

        expect(request.config.location.oauthRedirectUri).toBe(
            'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html',
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
            platform: 'windows', mode: 'create', purpose: 'backup', strategy: 'backup-only',
            scope: defaultExternalStorageScope('backup'), acknowledgements: [BACKUP_ONLY_ACKNOWLEDGEMENT],
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
        }
        const updated = { ...first, pinned: true }
        const second = { ...first, id: 'snapshot-2', logicalRevision: '2' as const }

        expect(mergeExternalHistoryItems([first], [updated, second])).toEqual([updated, second])
    })

    it('preserves pinned conflict metadata when a later root page repeats a snapshot', () => {
        const conflict = {
            id: 'snapshot-1', kind: 'conflict' as const, createdAtMs: '1' as const,
            logicalRevision: '1' as const, pinned: true, complete: false, verified: false,
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

    it('offers only same-sync preservation retry until the remote conflict copy is complete', () => {
        const localOnly = {
            id: 'conflict-1', connectionId: 'connection', detectedAtMs: '1' as const,
            localRevision: '8' as const, remoteRevision: null, preservation: 'local-only' as const,
            localLabel: 'Local snapshot', remoteLabel: 'Remote snapshot',
        }

        expect(externalConflictActions(localOnly)).toEqual(['retry-sync'])
        expect(externalConflictActions({
            ...localOnly,
            remoteRevision: '7',
            preservation: 'remote-complete',
        })).toEqual(['keep-local', 'use-remote'])
        expect(externalConflictActions({
            ...localOnly,
            preservation: 'remote-complete',
        })).toEqual([])
    })
})
