import { describe, expect, test } from 'vitest'
import source from './DeviceSyncSettings.svelte?raw'

describe('DeviceSyncSettings', () => {
    test('uses the unified controller and renders the three-card page structure', () => {
        expect(source).toContain('getProductionDeviceSyncController')
        expect(source).toContain('sync.share.title')
        expect(source).toContain('sync.devices.title')
        expect(source).toContain('sync.work.title')
        expect(source).not.toContain('PeerCloneSettings')
    })

    test('keeps receive actions blocked while sharing and avoids raw native errors', () => {
        expect(source).toContain('source.phase !== \'idle\'')
        expect(source).not.toContain('cause instanceof Error ? cause.message')
        expect(source).toContain('sync.lanWarning')
    })
})
