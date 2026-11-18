import { describe, expect, it } from 'vitest'
import { language } from 'src/lang'
import { NativeFileJobError } from '../storage/nativeFileJobs'
import { importIntakeRefusalText } from './refusalText'

describe('import intake refusal text', () => {
    it('names the plugin API version and maps read failures through the import reasons', () => {
        expect(importIntakeRefusalText({ code: 'plugin-api-version', version: '2.1' }))
            .toBe(language.risuNest.plugins.unsupportedApiVersionInstall.replace('{version}', '2.1'))
        expect(importIntakeRefusalText({ code: 'read-failed', error: new NativeFileJobError('source-read-failed', 'denied') }))
            .toBe(language.risuNest.importDialog.reasonIo)
        expect(importIntakeRefusalText({ code: 'read-failed', error: new NativeFileJobError('payload-too-large', 'synthetic', [], 'content') }))
            .toBe(language.risuNest.importDialog.reasonTooLarge.replace('{0}', language.risuNest.importDialog.tooLargeItems.content))
    })
})
