import { describe, expect, it } from 'vitest'

import peerCloneSettingsSource from './PeerCloneSettings.svelte?raw'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'

describe('peer delta settings surface', () => {
    it('keeps incremental update separate from destructive full clone', () => {
        expect(peerCloneSettingsSource).toContain('language.peerDelta.title')
        expect(peerCloneSettingsSource).toContain('getDesktopPeerDeltaController')
        expect(peerCloneSettingsSource).toContain(
            'acquirePersistentMutationFence: acquireDestructiveReplacementFence',
        )
        expect(peerCloneSettingsSource).toContain('controller.confirmDestructiveReplace()')
        expect(peerCloneSettingsSource).toContain('deltaController.pull(deltaPairingInput)')
    })

    it('documents source lifetime and safe first-pull behavior in English and Korean', () => {
        expect(languageEnglish.peerDelta.sourceOpenHelp).toContain('open')
        expect(languageEnglish.peerDelta.fullCloneRequired).toContain('full clone')
        expect(languageEnglish.peerDelta.divergenceHelp).toContain('never replaces')
        expect(languageKorean.peerDelta?.sourceOpenHelp).toContain('열어')
        expect(languageKorean.peerDelta?.fullCloneRequired).toContain('전체 복제')
        expect(languageKorean.peerDelta?.divergenceHelp).toContain('교체하지')
    })

    it('keeps delta actions disabled while a remounted controller reports a running pull', () => {
        expect(peerCloneSettingsSource).toContain(
            "const deltaOperationRunning = $derived(deltaBusy || deltaPullPhase === 'running')",
        )
        expect(peerCloneSettingsSource).toContain('disabled={!deltaTargetEnabled || deltaOperationRunning || !deltaPairingInput}')
        expect(peerCloneSettingsSource).toContain('disabled={!deltaSourceEnabled || deltaOperationRunning}')
    })

    it('uses translated invalid-link feedback for delta pairing input', () => {
        expect(peerCloneSettingsSource).toContain('deltaError = language.peerDelta.invalidLink')
        expect(languageEnglish.peerDelta.invalidLink).toContain('invalid')
        expect(languageKorean.peerDelta?.invalidLink).toContain('잘못')
    })

    it('keeps LAN as the P4 default and clears Named tunnel tokens on every terminal path', () => {
        expect(peerCloneSettingsSource).toContain("let deltaSourceMode = $state<'lan' | 'quick' | 'named'>('lan')")
        expect(peerCloneSettingsSource).toContain('deltaController.startQuickTunnel(sessionId)')
        expect(peerCloneSettingsSource).toContain(
            'deltaController.startNamedTunnel(sessionId, token, deltaNamedTunnelPublicBaseUrl)',
        )
        expect(peerCloneSettingsSource).toContain("if (deltaSourceMode !== 'named' || deltaSourceStatus.phase !== 'prepared')")
        expect(peerCloneSettingsSource.match(/deltaNamedTunnelToken = ''/g)?.length).toBeGreaterThanOrEqual(4)
    })
})
