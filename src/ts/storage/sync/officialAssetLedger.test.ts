import { describe, expect, it } from 'vitest'
import {
    createAccountScopedOfficialAssetLedger,
    createOfficialAssetLedger,
    type LedgerStorage,
} from './officialAssetLedger'

function memoryStorage(): LedgerStorage {
    const values = new Map<string, string>()
    return {
        getItem: (key) => values.get(key) ?? null,
        setItem: (key, value) => void values.set(key, value),
        removeItem: (key) => void values.delete(key),
    }
}

describe('createAccountScopedOfficialAssetLedger', () => {
    it('starts unrecorded and begins recording once the account id is known', () => {
        const storage = memoryStorage()
        let accountId: string | undefined
        const ledger = createAccountScopedOfficialAssetLedger(storage, () => accountId)

        ledger.record('assets/a.png', 'remote/assets/a.png')
        expect(ledger.publishedAs('assets/a.png')).toBeNull()

        accountId = 'acct-1'
        ledger.record('assets/a.png', 'remote/assets/a.png')
        expect(ledger.publishedAs('assets/a.png')).toBe('remote/assets/a.png')
        expect(createOfficialAssetLedger(storage, 'acct-1').publishedAs('assets/a.png'))
            .toBe('remote/assets/a.png')
    })

    it('keeps recorded keys separated per account id', () => {
        const storage = memoryStorage()
        let accountId: string | undefined = 'acct-1'
        const ledger = createAccountScopedOfficialAssetLedger(storage, () => accountId)
        ledger.record('assets/a.png', 'remote/a')
        ledger.recordCold('cold-1', 'digest-1')

        accountId = 'acct-2'
        expect(ledger.publishedAs('assets/a.png')).toBeNull()
        expect(ledger.coldDigest('cold-1')).toBeNull()

        accountId = 'acct-1'
        expect(ledger.publishedAs('assets/a.png')).toBe('remote/a')
        expect(ledger.coldDigest('cold-1')).toBe('digest-1')
    })
})
