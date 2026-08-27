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
})
