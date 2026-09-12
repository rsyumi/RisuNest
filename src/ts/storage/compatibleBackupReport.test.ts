import { describe, expect, it } from 'vitest'
import { languageEnglish } from '../../lang/en'
import { languageKorean } from '../../lang/ko'
import { formatCompatibilityBackupReport } from './compatibleBackupReport'
import type { NativeCompatibilityReport } from './nativeFileJobs'

const report: NativeCompatibilityReport = {
    target: 'pocketrisu',
    preserved: [
        {
            code: 'attachment-files',
            items: '9007199254740993',
            bytes: '18446744073709551615',
            affectedConversations: null,
        },
    ],
    converted: [
        {
            code: 'inlay-ids-remapped',
            items: '2',
            bytes: '0',
            affectedConversations: '0',
        },
    ],
    excluded: [],
}

describe('compatibility backup report', () => {
    it('renders exact native decimal values, known zero and unknown distinctly', () => {
        const text = formatCompatibilityBackupReport(report, {
            ...languageEnglish.portableBackup,
            ...languageEnglish.compatibilityBackupReport,
        })
        expect(text).toContain('Compatibility export report (PocketRisu)')
        expect(text).toContain('9,007,199,254,740,993')
        expect(text).toContain('18,446,744,073,709,551,615 B')
        expect(text).toContain('Affected conversations: Not determined')
        expect(text).toContain('Affected conversations: 0')
        expect(text).toContain('Excluded\nNone')
    })

    it('uses translated categories and a human-readable fallback without exposing codes', () => {
        const text = formatCompatibilityBackupReport(
            {
                ...report,
                target: 'risuai',
                excluded: [
                    {
                        code: 'future-internal-code',
                        items: '1',
                        bytes: '0',
                        affectedConversations: null,
                    },
                ],
            },
            {
                ...languageKorean.portableBackup,
                ...languageKorean.compatibilityBackupReport,
            },
        )
        expect(text).toContain('호환 내보내기 보고서 (RisuAI)')
        expect(text).toContain('첨부 파일')
        expect(text).toContain('인레이 식별자 변경')
        expect(text).toContain('기타 호환성 항목')
        expect(text).toContain('영향받는 대화: 집계 불가')
        expect(text).not.toContain('future-internal-code')
    })
})
