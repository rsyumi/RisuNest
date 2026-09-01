// @vitest-environment happy-dom

import { describe, expect, test } from 'vitest'

import triggerSource from './TriggerV2List.svelte?raw'
import observerSource from 'src/ts/observer.svelte.ts?raw'

describe('TriggerV2List export', () => {
    test('passes binary data to downloadFile instead of creating object URLs', () => {
        for (const source of [triggerSource, observerSource]) {
            expect(source).toContain('downloadFile')
            expect(source).toContain('new Uint8Array(await blob.arrayBuffer())')
            expect(source).not.toContain('downloadBlobWithObjectUrl')
        }
    })
})
