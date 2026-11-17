import { describe, expect, it } from 'vitest'
import { externalConnectionFolder, externalConnectionPlace, externalConnectionTitle, externalErrorMessage, externalFolderErrorKind, externalStorageStrings } from './strings'

const strings = externalStorageStrings('en')

describe('the connection title and place', () => {
    const connection = (providerId: 'webdav' | 'google_drive' | 'onedrive', endpoint: { authority: string; repositoryHint: string; accountHint?: string }) =>
        ({ providerId, endpoint: { providerId, warnings: [], remoteVerified: true, ...endpoint } })

    it('shows the server and folder of a self-hosted repository', () => {
        const webdav = connection('webdav', { authority: 'https://synthetic.invalid', repositoryHint: '/RisuNest', accountHint: 'synthetic-user' })
        expect(externalConnectionTitle(strings, webdav)).toBe('WebDAV · synthetic-user')
        expect(externalConnectionPlace(strings, webdav)).toBe('https://synthetic.invalid · /RisuNest')
    })

    it('leaves out the fixed API host of a signed-in service', () => {
        const drive = connection('google_drive', { authority: 'https://www.googleapis.com', repositoryHint: 'RisuNest', accountHint: 'synthetic@example.invalid' })
        expect(externalConnectionTitle(strings, drive)).toBe('Google Drive · synthetic@example.invalid')
        expect(externalConnectionPlace(strings, drive)).toBe('RisuNest')
        expect(externalConnectionPlace(strings, connection('onedrive', { authority: 'https://graph.microsoft.com', repositoryHint: '' }))).toBe('')
    })

    it('names the fixed app folders as the form does', () => {
        for (const locale of ['ko', 'en']) {
            const copy = externalStorageStrings(locale)
            const hidden = connection('google_drive', { authority: 'https://www.googleapis.com', repositoryHint: 'appDataFolder' })
            const appFolder = connection('onedrive', { authority: 'https://graph.microsoft.com', repositoryHint: 'special/approot' })
            expect(externalConnectionFolder(copy, hidden)).toBe(copy.options['google_drive.space.appDataFolder'])
            expect(externalConnectionPlace(copy, appFolder)).toBe(copy.options['onedrive.accountType.appFolder'])
        }
        expect(externalConnectionFolder(strings, connection('webdav', { authority: 'https://synthetic.invalid', repositoryHint: 'appDataFolder' }))).toBe('appDataFolder')
    })
})

describe('the storage usage copy', () => {
    it('names the repository data this device knows of without calling it a lower bound', () => {
        expect(externalStorageStrings('ko')).toMatchObject({ uploadedLowerBound: '이 기기에서 확인한 저장소 데이터', files: '파일 {0}개' })
        expect(strings).toMatchObject({ uploadedLowerBound: 'Repository data known to this device', files: '{0} files' })
    })

    it('says only what cleanup removes under the retention settings', () => {
        expect(externalStorageStrings('ko').retentionHelp).toBe('정리할 때 이 기기에서 만든 자동 백업 중 보관 개수와 보관 기간을 모두 넘긴 백업을 지웁니다.')
        expect(strings.retentionHelp).toBe('Cleanup removes automatic backups made on this device once they are past both the number of backups to keep and the days to keep them.')
    })
})

describe('externalErrorMessage', () => {
    it.each(['en', 'ko'])('explains QR failures in %s without an internal code', language => {
        const copy = externalStorageStrings(language)
        for (const code of ['qr-camera-permission-denied', 'qr-camera-permission-blocked']) {
            expect(externalErrorMessage(copy, new Error(code))).toBe(copy.cameraDenied)
        }
        for (const code of ['qr-camera-unavailable', 'qr-scan-busy', 'qr-scan-timeout']) {
            expect(externalErrorMessage(copy, new Error(code))).toBe(copy.cameraUnavailable)
        }
    })

    it('keeps native connection diagnostics and HTTP status in both languages', () => {
        for (const locale of ['ko', 'en']) {
            const copy = externalStorageStrings(locale)
            const detail = 'TLS certificate verification failed: certificate has expired'
            expect(externalErrorMessage(copy, { kind: 'endpointRejected', detail, httpStatus: 502 }))
                .toBe([copy.endpointRejected, 'HTTP 502', detail].join('\n'))
            expect(externalErrorMessage(copy, { code: 'endpointRejected', detail, message: detail, httpStatus: 502 }))
                .toBe([copy.endpointRejected, 'HTTP 502', detail].join('\n'))
        }
    })
    it('reads the kind of a rejected command and the code of a job error', () => {
        expect(externalErrorMessage(strings, { kind: 'storageFull', httpStatus: null, retryAtMs: null }))
            .toBe(strings.freeSpace)
        expect(externalErrorMessage(strings, { code: 'corrupt', message: 'x', action: 'none', retryable: false }))
            .toBe(`${strings.corrupted}\nx`)
    })

    it('tells a wrong recovery key apart from another repository at the location', () => {
        expect(externalErrorMessage(strings, { kind: 'recoveryKeyMismatch' })).toBe(strings.recoveryKeyMismatch)
        expect(externalErrorMessage(strings, { code: 'repositoryMismatch', message: 'x', action: 'none', retryable: false }))
            .toBe(`${strings.repositoryMismatch}\nx`)
    })

    it('names a repository this device has already connected', () => {
        expect(externalErrorMessage(strings, { kind: 'alreadyConnected' }))
            .toBe(strings.connectionAlreadyAdded)
    })

    it('reports unsupported operations without asking users to change the strategy', () => {
        const refused = { kind: 'unsupported', httpStatus: null, retryAtMs: null }
        expect(externalErrorMessage(strings, refused)).toBe(strings.unsupportedOperation)
        for (const locale of ['ko', 'en']) {
            const copy = externalStorageStrings(locale)
            const expected = copy.requestLengthRequired
            for (const failure of [{ kind: 'unsupported', httpStatus: 411 }, { code: 'unsupported', httpStatus: 411 }]) {
                expect(externalErrorMessage(copy, failure)).toBe(`${expected}\nHTTP 411`)
                expect(externalErrorMessage(copy, { ...failure,
                    message: 'This operation is unavailable for this repository.', detail: 'Length Required',
                })).toBe(`${expected}\nHTTP 411\nLength Required`)
            }
        }
    })

    it('shows the OAuth error code and provider description', () => {
        expect(externalErrorMessage(strings, {
            kind: 'reauthRequired', httpStatus: 400, retryAtMs: null,
            oauthError: 'invalid_grant', oauthErrorDescription: 'Bad Request: redirect_uri is invalid.',
        })).toBe([
            strings.reauthenticate,
            strings.oauthErrorCode.replace('{0}', 'invalid_grant'),
            strings.oauthErrorDescription.replace('{0}', 'Bad Request: redirect_uri is invalid.'),
            'HTTP 400',
        ].join('\n'))
        expect(externalErrorMessage(strings, {
            kind: 'reauthRequired', oauthError: '<script>alert(1)</script>',
        })).toBe([
            strings.reauthenticate,
            strings.oauthErrorCode.replace('{0}', '<script>alert(1)</script>'),
        ].join('\n'))
        // An asset download names the same stops with its own codes.
        expect(externalErrorMessage(strings, { code: 'external-reauth-required', status: 502, retryable: true })).toBe(strings.reauthenticate)
        expect(externalErrorMessage(strings, { code: 'external-key-locked', status: 502, retryable: false })).toBe(strings.unlockKey)
    })

    it('falls back for a local failure that carries no native kind', () => {
        expect(externalErrorMessage(strings, new Error('offline'))).toBe(`${strings.errorGeneric}\noffline`)
        expect(externalErrorMessage(strings, undefined)).toBe(strings.errorGeneric)
        expect(externalErrorMessage(strings, { kind: 'invented' })).toBe(strings.errorGeneric)
    })

    it('keeps both languages complete for every kind it maps', () => {
        const korean = externalStorageStrings('ko')
        const kinds = [
            'unauthorized', 'reauthRequired', 'notFound', 'preconditionFailed',
            'rateLimited', 'dailyQuotaExhausted', 'storageFull', 'fileTooLarge',
            'corrupt', 'unsupported', 'cancelled', 'transient', 'alreadyConnected',
            'folderNameConflict', 'folderCreateFailed', 'folderInaccessible',
            'folderNotRepository', 'folderUnsupportedLocation',
            'endpointRejected', 'deviceVaultUnavailable', 'clockSkew', 'folderTooLarge',
            'repositoryBusy', 'locationOccupied', 'authorizationTimedOut', 'localStorageFull',
            'localPermissionDenied', 'localFailure', 'repositoryKeyUnavailable', 'authorizationUnavailable',
        ]
        for (const kind of kinds) {
            expect(externalErrorMessage(korean, { kind })).not.toBe(korean.errorGeneric)
            expect(externalErrorMessage(korean, { kind })).not.toBe(externalErrorMessage(strings, { kind }))
        }
    })

    it('separates folder-row failures from creation and connection failures', () => {
        for (const kind of ['folderInaccessible', 'folderNotRepository', 'folderUnsupportedLocation']) {
            expect(externalFolderErrorKind({ kind })).toBe(true)
        }
        for (const kind of ['folderNameConflict', 'notFound', undefined]) {
            expect(externalFolderErrorKind(kind === undefined ? undefined : { kind })).toBe(false)
        }
    })
})
