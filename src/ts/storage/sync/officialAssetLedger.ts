const LEDGER_VERSION = 1

export interface OfficialAssetLedger {
    /** The key the account already holds this asset under, or null when it was never published. */
    publishedAs(key: string): string | null
    record(key: string, replacementKey: string): void
    /** Digest of the last published cold payload, or null when it was never published. */
    coldDigest(key: string): string | null
    recordCold(key: string, digest: string): void
    clear(): void
    reset(): void
}

interface LedgerRecord {
    version: number
    assets: Record<string, string>
    cold: Record<string, string>
}

export interface LedgerStorage {
    getItem(key: string): string | null
    setItem(key: string, value: string): void
    removeItem(key: string): void
}

function emptyRecord(): LedgerRecord {
    return { version: LEDGER_VERSION, assets: {}, cold: {} }
}

function parseRecord(raw: string | null): LedgerRecord {
    if (!raw) return emptyRecord()
    try {
        const parsed = JSON.parse(raw) as Partial<LedgerRecord>
        if (parsed?.version !== LEDGER_VERSION) return emptyRecord()
        return {
            version: LEDGER_VERSION,
            assets: { ...parsed.assets },
            cold: { ...parsed.cold },
        }
    } catch {
        return emptyRecord()
    }
}

/**
 * Remembers what an account already holds so an ordinary publish uploads only what changed.
 * Asset keys are content addressed, so a recorded key stays valid; cold payloads keep a digest
 * because their bytes change under a stable key.
 */
export function createOfficialAssetLedger(
    storage: LedgerStorage,
    accountId: string,
): OfficialAssetLedger {
    const storageKey = `officialPublishedAssets:${accountId}`
    let record = parseRecord(storage.getItem(storageKey))
    const persist = () => {
        try {
            storage.setItem(storageKey, JSON.stringify(record))
        } catch (error) {
            console.error('Failed to persist the official publication ledger', error)
        }
    }

    return {
        publishedAs: (key) => record.assets[key] ?? null,
        record(key, replacementKey) {
            if (record.assets[key] === replacementKey) return
            record.assets[key] = replacementKey
            persist()
        },
        coldDigest: (key) => record.cold[key] ?? null,
        recordCold(key, digest) {
            if (record.cold[key] === digest) return
            record.cold[key] = digest
            persist()
        },
        clear() {
            record = emptyRecord()
            storage.removeItem(storageKey)
        },
        reset() {
            record = emptyRecord()
        },
    }
}

export function createUnrecordedOfficialAssetLedger(): OfficialAssetLedger {
    return {
        publishedAs: () => null,
        record: () => undefined,
        coldDigest: () => null,
        recordCold: () => undefined,
        clear: () => undefined,
        reset: () => undefined,
    }
}

/**
 * Resolves the recording ledger from the live account id on every call, so a session that only
 * learns its account identity after the first pull still records replacement keys.
 */
export function createAccountScopedOfficialAssetLedger(
    storage: LedgerStorage,
    resolveAccountId: () => string | undefined,
): OfficialAssetLedger {
    const unrecorded = createUnrecordedOfficialAssetLedger()
    let boundId: string | null = null
    let bound: OfficialAssetLedger = unrecorded
    const resolve = (): OfficialAssetLedger => {
        const accountId = resolveAccountId()
        if (!accountId) {
            boundId = null
            bound = unrecorded
        } else if (accountId !== boundId) {
            boundId = accountId
            bound = createOfficialAssetLedger(storage, accountId)
        }
        return bound
    }
    return {
        publishedAs: (key) => resolve().publishedAs(key),
        record: (key, replacementKey) => resolve().record(key, replacementKey),
        coldDigest: (key) => resolve().coldDigest(key),
        recordCold: (key, digest) => resolve().recordCold(key, digest),
        clear: () => resolve().clear(),
        reset() {
            boundId = null
            bound = unrecorded
        },
    }
}
