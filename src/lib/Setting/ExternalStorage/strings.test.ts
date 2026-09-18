import { describe, expect, it } from 'vitest'
import { externalErrorMessage, externalStorageStrings } from './strings'

const strings = externalStorageStrings('en')

describe('externalErrorMessage', () => {
    it('reads the kind of a rejected command and the code of a job error', () => {
        expect(externalErrorMessage(strings, { kind: 'storageFull', httpStatus: null, retryAtMs: null }))
            .toBe(strings.freeSpace)
        expect(externalErrorMessage(strings, { code: 'corrupt', message: 'x', action: 'none', retryable: false }))
            .toBe(strings.corrupted)
    })

    it('names a repository this device has already connected', () => {
        expect(externalErrorMessage(strings, { kind: 'alreadyConnected' }))
            .toBe(strings.connectionAlreadyAdded)
    })

    it('reports unsupported operations without asking users to change the strategy', () => {
        const refused = { kind: 'unsupported', httpStatus: null, retryAtMs: null }
        expect(externalErrorMessage(strings, refused)).toBe(strings.unsupportedOperation)
    })

    it('falls back for a local failure that carries no native kind', () => {
        expect(externalErrorMessage(strings, new Error('offline'))).toBe(strings.errorGeneric)
        expect(externalErrorMessage(strings, undefined)).toBe(strings.errorGeneric)
        expect(externalErrorMessage(strings, { kind: 'invented' })).toBe(strings.errorGeneric)
    })

    it('keeps both languages complete for every kind it maps', () => {
        const korean = externalStorageStrings('ko')
        const kinds = [
            'unauthorized', 'reauthRequired', 'notFound', 'preconditionFailed',
            'rateLimited', 'dailyQuotaExhausted', 'storageFull', 'fileTooLarge',
            'corrupt', 'unsupported', 'cancelled', 'transient', 'alreadyConnected',
        ]
        for (const kind of kinds) {
            expect(externalErrorMessage(korean, { kind })).not.toBe(korean.errorGeneric)
            expect(externalErrorMessage(korean, { kind })).not.toBe(externalErrorMessage(strings, { kind }))
        }
    })
})
