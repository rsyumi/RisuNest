import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'

const source = readFileSync('src/lib/Setting/Pages/RisuNestAndroidPlatform.svelte', 'utf8')

describe('RisuNest Android platform settings', () => {
    it('hides without notification status and exposes the Android diagnostics controls', () => {
        expect(source).toContain('notificationStatus === null')
        expect(source).toContain('openNotificationSettings')
        expect(source).toContain('androidKeepAliveDuringGeneration')
        expect(source).toContain('webViewVersion')
        expect(source).toContain('transferMode')
        expect(source).toContain('keepAliveNeedsNotifications')
    })
})
