import { describe, expect, it } from 'vitest'

import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'

describe('peer delta settings language', () => {
    it('documents source lifetime and safe first-pull behavior in English and Korean', () => {
        expect(languageEnglish.peerDelta.sourceOpenHelp).toContain('open')
        expect(languageEnglish.peerDelta.fullCloneRequired).toContain('full clone')
        expect(languageEnglish.peerDelta.divergenceHelp).toContain('never replaces')
        expect(languageKorean.peerDelta?.sourceOpenHelp).toContain('열어')
        expect(languageKorean.peerDelta?.fullCloneRequired).toContain('전체 복제')
        expect(languageKorean.peerDelta?.divergenceHelp).toContain('교체하지')
    })

    it('translates invalid-link feedback for delta pairing input', () => {
        expect(languageEnglish.peerDelta.invalidLink).toContain('invalid')
        expect(languageKorean.peerDelta?.invalidLink).toContain('잘못')
    })
})

describe('peer bidirectional settings language', () => {
    it('describes the desktop transports and expired links in English and Korean', () => {
        expect(languageEnglish.peerBidirectional.sourceHelp).toContain('LAN or tunnel')
        expect(languageKorean.peerBidirectional?.sourceHelp).toContain('LAN 또는 터널')
        expect(languageEnglish.peerBidirectional.sourceUnavailable).toContain('fresh link')
        expect(languageKorean.peerBidirectional?.sourceUnavailable).toContain('새 링크')
    })

    it('keeps the Android LAN help free of tunnel wording', () => {
        expect(languageEnglish.peerBidirectional.sourceHelpLan).toContain('LAN session')
        expect(languageEnglish.peerBidirectional.sourceHelpLan).not.toContain('tunnel')
        expect(languageKorean.peerBidirectional?.sourceHelpLan).toContain('LAN 세션')
        expect(languageKorean.peerBidirectional?.sourceHelpLan).not.toContain('터널')
    })

    it('offers explicit whole-operation conflict winners', () => {
        expect(languageEnglish.peerBidirectional.keepLocal).toContain('this device')
        expect(languageEnglish.peerBidirectional.keepRemote).toContain('other device')
        expect(languageKorean.peerBidirectional?.keepLocal).toContain('이 기기')
        expect(languageKorean.peerBidirectional?.keepRemote).toContain('다른 기기')
        expect(languageEnglish.peerBidirectional.conflictReconnectHelp).toContain('fresh link')
        expect(languageKorean.peerBidirectional?.conflictReconnectHelp).toContain('새 링크')
    })

    it('describes durable backup and resume states', () => {
        expect(languageEnglish.peerBidirectional.backupCreated).toContain('lossless backup')
        expect(languageEnglish.peerBidirectional.resume).toContain('Resume')
        expect(languageKorean.peerBidirectional?.backupCreated).toContain('무손실 백업')
        expect(languageKorean.peerBidirectional?.resume).toContain('재개')
    })

    it('names retained-source and abandonment consequences', () => {
        expect(languageEnglish.peerBidirectional.sourceUnavailable).toContain('other device')
        expect(languageKorean.peerBidirectional?.sourceUnavailable).toContain('다른 기기')
        expect(languageEnglish.peerBidirectional.abandon).toContain('Abandon')
        expect(languageEnglish.peerBidirectional.abandonConfirm).toContain('committed data')
        expect(languageKorean.peerBidirectional?.abandon).toContain('포기')
        expect(languageKorean.peerBidirectional?.abandonConfirm).toContain('반영된 데이터')
    })

    it('localizes the losing backup side', () => {
        expect(languageEnglish.peerBidirectional.backupLocal).toContain('this device')
        expect(languageEnglish.peerBidirectional.backupRemote).toContain('other device')
    })
})
