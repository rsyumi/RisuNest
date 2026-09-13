import { describe, expect, it, vi } from 'vitest'

import {
    NativeOfficialPublicationFileUploader,
    type NativeOfficialPublicationCredential,
} from './nativeOfficialPublicationUpload'

describe('NativeOfficialPublicationFileUploader', () => {
    it('reauthenticates an ordinary 403 and retries the same file with its session', async () => {
        let sharedSession: string | null = null
        let credential: NativeOfficialPublicationCredential = {
            kind: 'risu-auth',
            token: 'old-token',
        }
        const invoke = vi.fn()
            .mockResolvedValueOnce({
                kind: 'reauthentication-needed',
                warning: 'please sign in',
                session: 'session-42',
                saveDate: '1000',
                status: 403,
                bytesUploaded: 21,
            })
            .mockResolvedValueOnce({
                kind: 'written',
                replacementKey: 'database/database.bin',
                session: 'session-42',
                saveDate: '1001',
                status: 200,
                bytesUploaded: 21,
                warning: null,
                reloadSession: false,
            })
        const reauthenticate = vi.fn(async () => {
            credential = { kind: 'risu-auth', token: 'new-token' }
        })
        const now = vi.fn().mockReturnValueOnce(1000).mockReturnValueOnce(1001)
        const onWarning = vi.fn()
        const uploader = new NativeOfficialPublicationFileUploader({
            onWarning,
            baseUrl: 'https://account.invalid',
            credential: () => credential,
            invoke,
            now,
            reauthenticate,
            getSession: () => sharedSession,
            setSession: (session) => {
                sharedSession = session
            },
        })

        await expect(uploader.upload({
            path: 'C:\\app\\exports\\snapshot.risudat',
            bytes: 21,
        })).resolves.toEqual({
            kind: 'written',
            replacementKey: 'database/database.bin',
            status: 200,
            bytesUploaded: 21,
            warning: null,
            reloadSession: false,
        })

        expect(reauthenticate).toHaveBeenCalledOnce()
        expect(sharedSession).toBe('session-42')
        expect(onWarning).toHaveBeenCalledWith('please sign in')
        expect(invoke.mock.calls).toEqual([
            ['official_publication_upload_file', {
                request: {
                    baseUrl: 'https://account.invalid',
                    credential: { kind: 'risu-auth', token: 'old-token' },
                    path: 'C:\\app\\exports\\snapshot.risudat',
                    saveDate: '1000',
                    session: null,
                },
            }],
            ['official_publication_upload_file', {
                request: {
                    baseUrl: 'https://account.invalid',
                    credential: { kind: 'risu-auth', token: 'new-token' },
                    path: 'C:\\app\\exports\\snapshot.risudat',
                    saveDate: '1001',
                    session: 'session-42',
                },
            }],
        ])
    })

    it('returns a warning 403 without reauthentication', async () => {
        let sharedSession: string | null = 'shared-session'
        const reauthenticate = vi.fn(async () => undefined)
        const uploader = new NativeOfficialPublicationFileUploader({
            baseUrl: 'https://account.invalid',
            credential: () => ({ kind: 'risu-auth', token: 'token' }),
            invoke: vi.fn(async () => ({
                kind: 'auth-warning',
                warning: 'quota exceeded',
                session: 'session-1',
                saveDate: '1000',
                status: 403,
                bytesUploaded: 21,
            })),
            now: () => 1000,
            reauthenticate,
            getSession: () => sharedSession,
            setSession: (session) => {
                sharedSession = session
            },
        })

        await expect(uploader.upload({ path: 'snapshot.risudat', bytes: 21 })).resolves.toEqual({
            kind: 'auth-warning',
            status: 403,
            bytesUploaded: 21,
            warning: 'quota exceeded',
        })
        expect(reauthenticate).not.toHaveBeenCalled()
        expect(sharedSession).toBe('session-1')
    })

    it('rejects a mismatched uploaded byte count', async () => {
        let sharedSession: string | null = null
        const uploader = new NativeOfficialPublicationFileUploader({
            baseUrl: 'https://account.invalid',
            credential: () => ({ kind: 'risu-auth', token: 'token' }),
            invoke: vi.fn(async () => ({
                kind: 'not-modified',
                replacementKey: 'database/database.bin',
                session: 'session-1',
                saveDate: '1000',
                status: 304,
                bytesUploaded: 20,
            })),
            now: () => 1000,
            reauthenticate: vi.fn(),
            getSession: () => sharedSession,
            setSession: (session) => {
                sharedSession = session
            },
        })

        await expect(uploader.upload({ path: 'snapshot.risudat', bytes: 21 }))
            .rejects.toThrow('Native official publication uploaded 20 bytes, expected 21')
    })
})
