import { describe, expect, it } from 'vitest'

import peerCloneSettingsSource from './PeerCloneSettings.svelte?raw'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'

describe('peer bidirectional settings surface', () => {
    it('presents bidirectional sync separately from full clone and one-way delta', () => {
        expect(peerCloneSettingsSource).toContain('language.peerBidirectional.title')
        expect(peerCloneSettingsSource).toContain('getDesktopPeerBidirectionalController')
        expect(peerCloneSettingsSource).toContain('bidirectionalController.sync')
        expect(peerCloneSettingsSource).toContain('bidirectionalController.resolve')
    })

    it('offers an explicit whole-operation winner for same-record conflicts', () => {
        expect(peerCloneSettingsSource).toContain("bidirectionalOperationPhase === 'awaitingConflict'")
        expect(peerCloneSettingsSource).toContain("resolveBidirectional('local')")
        expect(peerCloneSettingsSource).toContain("resolveBidirectional('remote')")
        expect(languageEnglish.peerBidirectional.keepLocal).toContain('this device')
        expect(languageEnglish.peerBidirectional.keepRemote).toContain('other device')
        expect(languageKorean.peerBidirectional?.keepLocal).toContain('이 기기')
        expect(languageKorean.peerBidirectional?.keepRemote).toContain('다른 기기')
    })

    it('shows durable backup and resume states', () => {
        expect(peerCloneSettingsSource).toContain('bidirectionalController.resume()')
        expect(peerCloneSettingsSource).toContain('language.peerBidirectional.backupCreated')
        expect(languageEnglish.peerBidirectional.backupCreated).toContain('lossless backup')
        expect(languageEnglish.peerBidirectional.resume).toContain('Resume')
        expect(languageKorean.peerBidirectional?.backupCreated).toContain('무손실 백업')
        expect(languageKorean.peerBidirectional?.resume).toContain('재개')
    })

    it('locks overlapping controls for retained operations and can dismiss a terminal result', () => {
        expect(peerCloneSettingsSource).toContain('bidirectionalOperationRetained')
        expect(peerCloneSettingsSource).toContain('disabled={!bidirectionalEnabled || bidirectionalBusy || bidirectionalOperationRetained}')
        expect(peerCloneSettingsSource).toContain('bidirectionalOperationRetained || !bidirectionalPairingInput')
        expect(peerCloneSettingsSource).toContain('bidirectionalOperationRetained || device.revoked')
        expect(peerCloneSettingsSource).toContain('bidirectionalController.acknowledge()')
        expect(peerCloneSettingsSource).toContain('language.peerBidirectional.acknowledge')
        expect(peerCloneSettingsSource).toContain("bidirectionalOperationPhase === 'sourceUnavailable'")
        expect(languageEnglish.peerBidirectional.sourceUnavailable).toContain('other device')
        expect(languageKorean.peerBidirectional?.sourceUnavailable).toContain('다른 기기')
    })

    it('localizes the losing backup side', () => {
        expect(peerCloneSettingsSource).toContain('language.peerBidirectional.backupLocal')
        expect(peerCloneSettingsSource).toContain('language.peerBidirectional.backupRemote')
        expect(languageEnglish.peerBidirectional.backupLocal).toContain('this device')
        expect(languageEnglish.peerBidirectional.backupRemote).toContain('other device')
    })
})
