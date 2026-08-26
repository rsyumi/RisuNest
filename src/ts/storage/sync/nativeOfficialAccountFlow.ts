import type { Database } from '../database.svelte'
import type { NativeAppKv } from '../nativeAppKv'
import type { DataRevision } from '../persistentDataStore'
import type { PinnedPublication } from '../saveCoordinator'
import type { OfficialPullResult } from './officialAccountSnapshot'

export const nativeOfficialAccountKeys = {
    credential: 'official-account.credential.v1',
    association: 'official-account.association.v1',
    assetLedger: 'official-account.asset-ledger.v1',
} as const

export type NativeOfficialAccountCredential = NonNullable<Database['account']>

interface NativeOfficialAdapter {
    pull(): Promise<OfficialPullResult>
    pin(revision: DataRevision): Promise<PinnedPublication>
    resetAccountAssociation(accountId: string | null): void
}

export interface NativeOfficialAccountFlowDependencies {
    appKv: NativeAppKv
    adapter: NativeOfficialAdapter
    initialCredential: NativeOfficialAccountCredential | null
    flushPendingData(reason: string): Promise<void>
    getRevision(): DataRevision
    restart(): Promise<void>
    setRouting(credential: NativeOfficialAccountCredential | null): void | Promise<void>
    clearLegacyFallback(): void
    flushMetadata(): Promise<void>
    resetMetadata(): void
    resetAccountSession(): void
    nativeRestore?(credential: NativeOfficialAccountCredential): Promise<
        OfficialPullResult | { kind: 'compatibility-fallback' }
    >
}

export interface NativeOfficialAccountFlow {
    login(credential: NativeOfficialAccountCredential): Promise<NativeOfficialAccountCredential>
    getToken(): string | null
    reauthenticate(loginResult: string): Promise<NativeOfficialAccountCredential>
    restore(): Promise<OfficialPullResult>
    publish(signal?: AbortSignal): Promise<void>
    logout(): Promise<void>
}

export interface NativeOfficialAccountFlowService {
    flow: NativeOfficialAccountFlow
    snapshotRequestReauthentication: {
        reauthenticate(loginResult: string): Promise<NativeOfficialAccountCredential>
    }
}

export function normalizeNativeOfficialAccountCredential(
    value: unknown,
): NativeOfficialAccountCredential | null {
    if (!value || typeof value !== 'object') return null
    const credential = value as Partial<NativeOfficialAccountCredential>
    if (typeof credential.id !== 'string' || typeof credential.token !== 'string') return null
    const data = credential.data && typeof credential.data === 'object' ? credential.data : {}
    return {
        id: credential.id,
        token: credential.token,
        data: {
            ...(typeof data.refresh_token === 'string' ? { refresh_token: data.refresh_token } : {}),
            ...(typeof data.access_token === 'string' ? { access_token: data.access_token } : {}),
            ...(typeof data.expires_in === 'number' ? { expires_in: data.expires_in } : {}),
        },
        ...(typeof credential.kei === 'boolean' ? { kei: credential.kei } : {}),
    }
}

export function createNativeOfficialAccountFlowService(
    dependencies: NativeOfficialAccountFlowDependencies,
): NativeOfficialAccountFlowService {
    let credential = dependencies.initialCredential
    let credentialGeneration = 0
    let operationTail = Promise.resolve()
    const serialize = <T>(operation: () => Promise<T>): Promise<T> => {
        const result = operationTail.then(operation, operation)
        operationTail = result.then(() => undefined, () => undefined)
        return result
    }
    const login = async (input: NativeOfficialAccountCredential) => {
        const nextCredential = normalizeNativeOfficialAccountCredential(input)
        if (!nextCredential) throw new Error('Invalid native official account credential')
        const previousCredential = credential
        const accountChanged = previousCredential?.id !== nextCredential.id
        try {
            if (accountChanged) dependencies.resetAccountSession()
            await dependencies.appKv.set(nativeOfficialAccountKeys.credential, nextCredential)
            await dependencies.setRouting(nextCredential)
            if (accountChanged) {
                dependencies.adapter.resetAccountAssociation(nextCredential.id)
            }
        } catch (error) {
            try {
                if (previousCredential) {
                    await dependencies.appKv.set(
                        nativeOfficialAccountKeys.credential,
                        previousCredential,
                    )
                } else {
                    await dependencies.appKv.remove(nativeOfficialAccountKeys.credential)
                }
            } catch {}
            try {
                await dependencies.setRouting(previousCredential)
            } catch {}
            if (accountChanged) {
                try {
                    dependencies.adapter.resetAccountAssociation(previousCredential?.id ?? null)
                } catch {}
            }
            throw error
        }
        credential = nextCredential
        credentialGeneration += 1
        return nextCredential
    }
    const parseCredential = (loginResult: string): NativeOfficialAccountCredential => {
        let parsed: unknown
        try {
            parsed = JSON.parse(loginResult)
        } catch (error) {
            throw new Error('Invalid native official account credential')
        }
        const parsedCredential = normalizeNativeOfficialAccountCredential(parsed)
        if (!parsedCredential) throw new Error('Invalid native official account credential')
        return parsedCredential
    }
    const reauthenticate = (loginResult: string) => login(parseCredential(loginResult))
    const flow: NativeOfficialAccountFlow = {
        login(input) {
            return serialize(() => login(input))
        },
        getToken() {
            return credential?.token ?? null
        },
        reauthenticate(loginResult) {
            const expectedGeneration = credentialGeneration
            const expectedAccountId = credential?.id
            return serialize(() => {
                if (!expectedAccountId
                    || credentialGeneration !== expectedGeneration
                    || credential?.id !== expectedAccountId) {
                    throw new Error('Native official account session changed during reauthentication')
                }
                return reauthenticate(loginResult)
            })
        },
        restore() {
            return serialize(async () => {
                if (!credential) throw new Error('Native official account login is required')
                await dependencies.flushPendingData('native-official-restore')
                const nativeResult = dependencies.nativeRestore
                    ? await dependencies.nativeRestore(credential)
                    : await dependencies.adapter.pull()
                const result = nativeResult.kind === 'compatibility-fallback'
                    ? await dependencies.adapter.pull()
                    : nativeResult
                if (result.kind === 'activated') {
                    let failed = false
                    let originalError: unknown
                    try {
                        await dependencies.flushMetadata()
                    } catch (error) {
                        failed = true
                        originalError = error
                    }
                    try {
                        await dependencies.restart()
                    } catch (error) {
                        if (!failed) {
                            failed = true
                            originalError = error
                        }
                    }
                    if (failed) throw originalError
                }
                return result
            })
        },
        publish(signal) {
            return serialize(async () => {
                if (!credential) throw new Error('Native official account login is required')
                await dependencies.flushPendingData('native-official-publish')
                let publication: PinnedPublication | null = null
                let failed = false
                let originalError: unknown
                try {
                    publication = await dependencies.adapter.pin(dependencies.getRevision())
                    await publication.publish(signal)
                } catch (error) {
                    failed = true
                    originalError = error
                } finally {
                    if (publication) {
                        try {
                            await publication.dispose()
                        } catch (error) {
                            if (!failed) {
                                failed = true
                                originalError = error
                            }
                        }
                    }
                    try {
                        await dependencies.flushMetadata()
                    } catch (error) {
                        if (!failed) {
                            failed = true
                            originalError = error
                        }
                    }
                }
                if (failed) throw originalError
            })
        },
        logout() {
            return serialize(async () => {
                dependencies.clearLegacyFallback()
                dependencies.resetAccountSession()
                await dependencies.flushMetadata()
                await Promise.all(Object.values(nativeOfficialAccountKeys).map(
                    (key) => dependencies.appKv.remove(key),
                ))
                dependencies.resetMetadata()
                credential = null
                credentialGeneration += 1
                dependencies.adapter.resetAccountAssociation(null)
                await dependencies.setRouting(null)
            })
        },
    }
    return {
        flow,
        snapshotRequestReauthentication: {
            reauthenticate(loginResult) {
                const currentAccountId = credential?.id
                const nextCredential = parseCredential(loginResult)
                if (!currentAccountId || nextCredential.id !== currentAccountId) {
                    throw new Error('Native official account changed during snapshot reauthentication')
                }
                return login(nextCredential)
            },
        },
    }
}

export function createNativeOfficialAccountFlow(
    dependencies: NativeOfficialAccountFlowDependencies,
): NativeOfficialAccountFlow {
    return createNativeOfficialAccountFlowService(dependencies).flow
}

let configuredFlow: NativeOfficialAccountFlow | null = null

export function configureNativeOfficialAccountFlow(flow: NativeOfficialAccountFlow | null): void {
    configuredFlow = flow
}

export function getNativeOfficialAccountFlow(): NativeOfficialAccountFlow {
    if (!configuredFlow) throw new Error('Native official account flow is not configured')
    return configuredFlow
}
