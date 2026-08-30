import { describe, expect, it } from 'vitest'

import source from './PeerCloneAndroidSettings.svelte?raw'

describe('Android P4 settings', () => {
    it('offers LAN-only logical delta source and target controls', () => {
        expect(source).toContain("platform: 'android'")
        expect(source).toContain('deltaController.prepare()')
        expect(source).toContain('deltaController.start(deltaSourceStatus.sessionId!)')
        expect(source).toContain('deltaController.pull(deltaPairingInput)')
        expect(source).toContain('!deltaCapabilities.tunnelReady')
        expect(source).not.toContain('deltaController.startQuickTunnel')
        expect(source).not.toContain('deltaController.startNamedTunnel')
        expect(source).toContain("!['idle', 'stopped'].includes(deltaSourceStatus.phase)")
    })
})
