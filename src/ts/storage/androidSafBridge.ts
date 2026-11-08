import { withLibraryFileSelection } from './libraryFileOperation'
import '../androidNativeControl'
import { BackupSourceCopyError } from './backupSourceCopyFailure'
import type { NativeFileJobSource } from './nativeFileJobs'

const SPOOL_EVENT = 'risu-android-spool-ready'
const BACKUP_SOURCE_EVENT = 'risu-android-backup-source-picked'
const PORTABLE_SOURCE_EVENT = 'risu-android-portable-source-picked'
const LEGACY_BACKUP_SOURCE_EVENT = 'risu-android-legacy-backup-source-picked'
const DESTINATION_EVENT = 'risu-android-saf-destination'
const PROGRESS_EVENT = 'risu-android-saf-progress'
const activeDestinationRequestIds = new Set<string>()

export interface AndroidSpoolReady {
    sourceType?: 'androidSeekable' | 'androidSpool'
    token: string
    displayName: string
    bytes: number
    totalBytes?: number | null
    importDestination?: 'character' | 'module' | null
}

export interface AndroidSpoolFailure {
    displayName: string
    code: string
}

export interface AndroidSpoolBatch {
    requestId: string
    ready: AndroidSpoolReady[]
    failures: AndroidSpoolFailure[]
}

export interface AndroidSpoolListenerDependencies {
    takePendingBatch(): AndroidSpoolBatch | null | undefined
    clearPendingBatch(): void
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
}

const productionSpoolListenerDependencies: AndroidSpoolListenerDependencies = {
    takePendingBatch: () => {
        const target = window as Window & { tauriOpenedFileSpools?: AndroidSpoolBatch }
        const batch = target.tauriOpenedFileSpools
        delete target.tauriOpenedFileSpools
        return batch
    },
    clearPendingBatch: () => {
        delete (window as Window & { tauriOpenedFileSpools?: AndroidSpoolBatch })
            .tauriOpenedFileSpools
    },
    addEventListener: (name, listener) => window.addEventListener(name, listener),
    removeEventListener: (name, listener) => window.removeEventListener(name, listener),
}

export function listenAndroidSpoolBatches(
    listener: (batch: AndroidSpoolBatch) => void,
    dependencies: AndroidSpoolListenerDependencies = productionSpoolListenerDependencies,
): () => void {
    const onReady = (event: Event) => {
        const batch = (event as CustomEvent<AndroidSpoolBatch>).detail
        if (batch) {
            dependencies.clearPendingBatch()
            listener(batch)
        }
    }
    dependencies.addEventListener(SPOOL_EVENT, onReady)
    const initial = dependencies.takePendingBatch()
    if (initial) queueMicrotask(() => listener(initial))
    return () => dependencies.removeEventListener(SPOOL_EVENT, onReady)
}

export interface AndroidSpoolConsumer {
    restore(input: { source: NativeFileJobSource; displayName: string }): Promise<void>
    unsupported(source: AndroidSpoolReady): void
    failed?(failure: AndroidSpoolFailure): void
}

// Content-spool suffix subset. Together with the database restore suffixes
// checked in consumeAndroidSpoolBatch below, it forms the Kotlin
// NATIVE_FILE_JOB_SPOOL_SUFFIXES allowlist; the alignment is pinned by the
// shared taxonomy golden fixture (tests/fixtures/nativeFileTaxonomyV1Golden.json).
export function isAndroidNativeContentSpool(source: Pick<AndroidSpoolReady, 'displayName'>): boolean {
    const displayName = source.displayName.toLocaleLowerCase('en-US')
    return displayName.endsWith('.json')
        || displayName.endsWith('.charx')
        || displayName.endsWith('.jpg')
        || displayName.endsWith('.jpeg')
        || displayName.endsWith('.png')
        || displayName.endsWith('.risum')
        || displayName.endsWith('.lorebook')
}

export async function consumeAndroidSpoolBatch(
    batch: AndroidSpoolBatch,
    consumer: AndroidSpoolConsumer,
): Promise<void> {
    for (const failure of batch.failures) consumer.failed?.(failure)
    for (const source of batch.ready) {
        const lowerName = source.displayName.toLocaleLowerCase('en-US')
        if (!['.risunest', '.risudat', '.bin'].some((extension) => lowerName.endsWith(extension))) {
            consumer.unsupported(source)
            continue
        }
        await consumer.restore({
            source: { type: 'androidSpool', token: source.token },
            displayName: source.displayName,
        })
    }
}

export interface AndroidSafDestinationRequest {
    sourcePath: string
    suggestedName: string
    signal?: AbortSignal
    onProgress?(progress: AndroidSafProgress): void
    deferAcknowledgement?: boolean
    reservation?: AndroidSafDestinationReservation
}

export interface AndroidSafDestinationReservation {
    requestId: string
    readonly jobId?: string
    bind(exportId: string): Promise<void>
    release(): Promise<void>
}

export type AndroidSafExportKind = NonNullable<AndroidSafDestinationEvent['sourceKind']>

export function bindAndroidSafDestinationSource(reservation: AndroidSafDestinationReservation, sourcePath: string): Promise<void> {
    const pieces = sourcePath.replaceAll('\\', '/').split('/')
    const name = pieces.at(-1) === 'archive.zip.part' ? pieces.at(-2) : pieces.at(-1)
    const exportId = name?.match(/(?:^|-)([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})(?:\.|$)/)?.[1]
    if (!exportId) return Promise.reject(new Error('Android SAF source has no export identity'))
    return reservation.bind(exportId)
}

export interface AndroidSafProgress {
    requestId: string
    operation: 'source-copy' | 'destination-copy'
    copiedBytes: number
    totalBytes: number | null
    token: string | null
}

export type AndroidBackupCopyProgress = Pick<AndroidSafProgress, 'copiedBytes' | 'totalBytes'>

export interface AndroidSafDestinationResult {
    requestId?: string
    bytes: number
    warningCodes: string[]
}

export interface AndroidSafDestinationEvent {
    requestId: string
    exportId?: string
    sourceKind?: 'risuSave' | 'legacyBackup' | 'screenshot' | 'download'
    state: 'succeeded' | 'failed' | 'cancelled'
    bytes?: number | null
    code?: string | null
    message?: string | null
    warningCodes: string[]
    publicationPrerequisitesComplete?: boolean
    copyStarted?: boolean
}

type NativeReply<T> = T | Promise<T>

export interface AndroidSafJavascriptBridge {
    copyReservedExport?(requestId: string, sourcePath: string, jobId: string): NativeReply<void>
    reserveExport?(requestId: string, suggestedName: string, sourceKind: AndroidSafExportKind): NativeReply<void>
    bindExport?(requestId: string, exportId: string): NativeReply<boolean>
    releaseExport?(requestId: string): NativeReply<boolean>
    copyExport(
        requestId: string,
        sourcePath: string,
        suggestedName: string,
    ): NativeReply<void>
    cancelExport?(requestId: string): NativeReply<boolean | void>
    cancelSource?(requestId: string): NativeReply<void>
    pickBackupSource?(requestId: string): NativeReply<void>
    pickPortableBackupSource?(requestId: string): NativeReply<void>
    discardPortableSource?(token: string): NativeReply<boolean>
    materializeBackupSource?(token: string, onProgress?: (progress: AndroidBackupCopyProgress) => void): NativeReply<string>
    pickContentSource?(
        requestId: string,
        destination: 'character' | 'module',
    ): NativeReply<void>
    pickLegacyBackupSource?(requestId: string): NativeReply<void>
    discardSource?(token: string): NativeReply<boolean>
    getActiveSourceRequestIds?(): NativeReply<string>
    getExportStatus?(): NativeReply<string | null>
    getExportSourceId?(): NativeReply<string | null>
    markExportPublicationReady?(requestId: string): NativeReply<boolean>
    acknowledgeExport?(requestId: string): NativeReply<boolean>
}

export interface AndroidSafSourcePickerOptions {
    signal?: AbortSignal
    onProgress?(progress: AndroidSafProgress): void
    /** Reports the picked file's name and size before the spool token is returned. */
    onSource?(source: { displayName: string; bytes: number }): void
}

export interface AndroidSafContentSourcePickerOptions
    extends AndroidSafSourcePickerOptions {
    destination: 'character' | 'module'
}

export interface AndroidSafSourcePickerDependencies {
    createRequestId(): string
    bridge: AndroidSafJavascriptBridge
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
}

export interface AndroidSafDestinationDependencies {
    createRequestId(): string
    bridge: AndroidSafJavascriptBridge
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
}

export function isAndroidSafFileJobsEnabled(
    bridge: unknown = (window as Window & {
        RisuSafBridge?: AndroidSafJavascriptBridge
    }).RisuSafBridge,
): boolean {
    return !!bridge
}

function productionBridge(): AndroidSafJavascriptBridge {
    const bridge = (window as Window & {
        RisuSafBridge?: AndroidSafJavascriptBridge
    }).RisuSafBridge
    if (!bridge) throw new Error('Android SAF bridge is unavailable')
    return bridge
}

const productionDependencies: AndroidSafDestinationDependencies = {
    createRequestId: () => crypto.randomUUID(),
    get bridge() {
        return productionBridge()
    },
    addEventListener: (name, listener) => window.addEventListener(name, listener),
    removeEventListener: (name, listener) => window.removeEventListener(name, listener),
}

export class AndroidSafDestinationError extends Error {
    constructor(
        readonly requestId: string,
        readonly code: string,
        readonly warningCodes: string[],
        message: string,
    ) {
        super(message)
        this.name = 'AndroidSafDestinationError'
    }
}

export class AndroidSafSourceError extends Error {
    constructor(readonly code: string, message: string) {
        super(message)
        this.name = 'AndroidSafSourceError'
    }
}

interface AndroidSafSourcePickerConfig {
    eventName: string
    pickMethod:
        | 'pickBackupSource'
        | 'pickPortableBackupSource'
        | 'pickLegacyBackupSource'
        | 'pickContentSource'
    pickerUnavailableMessage: string
    acceptExtension: string | string[]
    cancelMessage: string
    importDestination?: 'character' | 'module'
    custody?: boolean
}

type AndroidSpoolDiscard = (source: AndroidSpoolReady) => unknown

function acceptsSpool(config: AndroidSafSourcePickerConfig, source: AndroidSpoolReady): boolean {
    const extensions = Array.isArray(config.acceptExtension) ? config.acceptExtension : [config.acceptExtension]
    return extensions.some(ext => source.displayName.toLocaleLowerCase('en-US').endsWith(ext))
}

function pickAndroidSpoolSource(
    config: AndroidSafSourcePickerConfig,
    options: AndroidSafSourcePickerOptions,
    dependencies: AndroidSafSourcePickerDependencies,
): Promise<NativeFileJobSource | null> {
    return receiveAndroidSpoolBatch(config, options, dependencies, async (batch, discard) => {
        const failure = batch.failures[0]
        if (failure) throw new AndroidSafSourceError(failure.code, `${failure.displayName}: ${failure.code}`)
        const source = batch.ready[0]
        if (!source) return null
        if (!acceptsSpool(config, source)) {
            await Promise.resolve().then(() => discard(source)).catch(() => false)
            throw new AndroidSafSourceError('unsupported-format', `${source.displayName}: unsupported-format`)
        }
        options.onSource?.({
            displayName: source.displayName,
            bytes: source.bytes,
        })
        if (config.custody && source.sourceType !== 'androidSeekable' && source.sourceType !== 'androidSpool') {
            await Promise.resolve().then(() => discard(source)).catch(() => false)
            throw new AndroidSafSourceError('source-reselect-required', 'Backup source custody is unavailable')
        }
        return { type: config.custody ? source.sourceType! : 'androidSpool', token: source.token }
    })
}

function receiveAndroidSpoolBatch<T>(
    config: AndroidSafSourcePickerConfig,
    options: AndroidSafSourcePickerOptions,
    dependencies: AndroidSafSourcePickerDependencies,
    accept: (batch: AndroidSpoolBatch, discard: AndroidSpoolDiscard) => Promise<T>,
): Promise<T> {
    if (options.signal?.aborted) {
        return Promise.reject(
            new DOMException(config.cancelMessage, 'AbortError'),
        )
    }
    const requestId = dependencies.createRequestId()
    return withLibraryFileSelection(() => new Promise<T>((resolve, reject) => {
        let settled = false
        let receiving = false
        let aborted = false
        const cleanup = () => {
            options.signal?.removeEventListener('abort', onAbort)
            dependencies.removeEventListener(config.eventName, onEvent)
            if (options.onProgress) {
                dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
            }
        }
        const finish = (callback: () => void) => {
            if (settled) return
            settled = true
            cleanup()
            callback()
        }
        const onAbort = () => {
            if (settled) return
            aborted = true
            options.signal?.removeEventListener('abort', onAbort)
            if (options.onProgress) {
                dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
            }
            try {
                void Promise.resolve(dependencies.bridge.cancelSource?.(requestId)).catch(() => {})
            } catch {}
            // Cancellation is not a terminal receipt; retain the original source listener.
        }
        const onEvent = async (event: Event) => {
            const batch = (event as CustomEvent<AndroidSpoolBatch>).detail
            if (!batch || batch.requestId !== requestId || settled || receiving) return
            receiving = true
            const discard = (source: AndroidSpoolReady) => config.custody && source.sourceType !== 'androidSpool'
                ? dependencies.bridge.discardPortableSource?.(source.token)
                : dependencies.bridge.discardSource?.(source.token)
            if (aborted) {
                let cleanupFailed = false
                for (const source of batch.ready) {
                    if (
                        await Promise.resolve().then(() => discard(source)).catch(() => false) !==
                        true
                    ) {
                        cleanupFailed = true
                    }
                }
                finish(() =>
                    cleanupFailed
                        ? reject(
                              new AndroidSafSourceError(
                                  'cleanup-failed',
                                  'Cancelled Android backup source could not be cleaned up',
                              ),
                          )
                        : reject(
                              new DOMException(
                                  config.cancelMessage,
                                  'AbortError',
                              ),
                          ),
                )
                return
            }
            await accept(batch, discard).then(
                value => finish(() => resolve(value)),
                error => finish(() => reject(error)),
            )
        }
        const onProgress = (event: Event) => {
            const progress = (event as CustomEvent<AndroidSafProgress>).detail
            if (
                progress?.requestId === requestId &&
                progress.operation === 'source-copy'
            ) {
                options.onProgress?.(progress)
            }
        }
        dependencies.addEventListener(config.eventName, onEvent)
        if (options.onProgress) {
            dependencies.addEventListener(PROGRESS_EVENT, onProgress)
        }
        options.signal?.addEventListener('abort', onAbort, { once: true })
        try {
            const pick = dependencies.bridge[config.pickMethod]
            if (!pick) throw new Error(config.pickerUnavailableMessage)
            const started = config.pickMethod === 'pickContentSource'
                ? dependencies.bridge.pickContentSource?.(requestId, config.importDestination!)
                : (pick as (requestId: string) => NativeReply<void>).call(dependencies.bridge, requestId)
            void Promise.resolve(started).catch((error) => finish(() => reject(error)))
        } catch (error) {
            finish(() => reject(error))
        }
    }))
}


/** The native picker name is retained as an Android bridge command, not a file-format fallback. */
export function pickAndroidBackupSource(
    options: AndroidSafSourcePickerOptions = {},
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<NativeFileJobSource | null> {
    return pickAndroidSpoolSource(
        {
            eventName: BACKUP_SOURCE_EVENT,
            pickMethod: 'pickBackupSource',
            pickerUnavailableMessage: 'Android backup picker is unavailable',
            acceptExtension: ['.risunest', '.risudat', '.bin'],
            cancelMessage: 'Android backup selection was cancelled',
        },
        options,
        dependencies,
    )
}

export function pickAndroidPortableBackupSource(
    options: AndroidSafSourcePickerOptions = {},
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<NativeFileJobSource | null> {
    return pickAndroidSpoolSource({
        eventName: PORTABLE_SOURCE_EVENT,
        pickMethod: 'pickPortableBackupSource', custody: true,
        pickerUnavailableMessage: 'Android backup picker is unavailable',
        acceptExtension: ['.risunest', '.risudat', '.bin'],
        cancelMessage: 'Android backup selection was cancelled',
    }, options, dependencies)
}

export async function materializeAndroidBackupSource(
    token: string,
    options: Pick<AndroidSafSourcePickerOptions, 'signal' | 'onProgress'> = {},
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): Promise<NativeFileJobSource> {
    const cancelled = () => new DOMException('Android backup copy was cancelled', 'AbortError')
    if (options.signal?.aborted) throw cancelled()
    let stopWaiting!: (error: DOMException) => void
    const aborted = new Promise<never>((_resolve, reject) => { stopWaiting = reject })
    const onAbort = () => {
        // Releasing custody cancels the native copy, and the page stops waiting for its reply.
        void Promise.resolve().then(() => bridge.discardPortableSource?.(token)).catch(() => {})
        stopWaiting(cancelled())
    }
    options.signal?.addEventListener('abort', onAbort, { once: true })
    let copy: Promise<string | null | undefined> = Promise.resolve(undefined)
    try {
        copy = Promise.resolve(bridge.materializeBackupSource?.(token, progress => {
            if (!options.signal?.aborted) options.onProgress?.({ ...progress, requestId: token, token, operation: 'source-copy' })
        }))
        const response = await Promise.race([copy, aborted])
        if (options.signal?.aborted) throw cancelled()
        if (!response) throw new AndroidSafSourceError('source-reselect-required', 'Backup source custody is unavailable')
        const batch = JSON.parse(response) as AndroidSpoolBatch
        const failure = batch.failures[0]
        if (failure) throw new BackupSourceCopyError(failure.code)
        if (batch.ready.length !== 1) throw new AndroidSafSourceError('source-reselect-required', 'Backup source copy is unavailable')
        return {type: 'androidSpool', token: batch.ready[0].token}
    } catch (error) {
        if (!options.signal?.aborted) throw error
        // A copy that still finishes after cancellation is removed whenever its reply arrives.
        void copy.then(response => response && Promise.all((JSON.parse(response) as AndroidSpoolBatch).ready
            .map(source => bridge.discardSource?.(source.token)))).catch(() => {})
        throw cancelled()
    } finally {
        options.signal?.removeEventListener('abort', onAbort)
    }
}

export function pickAndroidLegacyBackupSource(
    options: AndroidSafSourcePickerOptions = {},
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<NativeFileJobSource | null> {
    return pickAndroidSpoolSource({
        eventName: LEGACY_BACKUP_SOURCE_EVENT,
        pickMethod: 'pickLegacyBackupSource',
        pickerUnavailableMessage: 'Android legacy backup picker is unavailable',
        acceptExtension: '.bin',
        cancelMessage: 'Android legacy backup selection was cancelled',
    }, options, dependencies)
}

function androidSafAbortError(
    requestId: string,
    detail?: Pick<AndroidSafDestinationEvent, 'code' | 'message' | 'warningCodes'>,
): DOMException & {
    requestId: string
    code?: string | null
    warningCodes: string[]
} {
    const error = new DOMException(
        detail?.message ?? 'Android SAF export was cancelled',
        'AbortError',
    )
    // DOMException.prototype.code is a getter without a setter, so assigning it throws.
    const own = (value: unknown) => ({ value, enumerable: true, writable: true, configurable: true })
    return Object.defineProperties(error, {
        requestId: own(requestId),
        code: own(detail?.code),
        warningCodes: own(detail?.warningCodes ?? []),
    }) as DOMException & {
        requestId: string
        code?: string | null
        warningCodes: string[]
    }
}

export function isAndroidSafDestinationRequestActive(requestId: string): boolean {
    return activeDestinationRequestIds.has(requestId)
}

export function listenAndroidSafDestinationEvents(
    listener: (event: AndroidSafDestinationEvent) => void,
    dependencies: Pick<AndroidSafDestinationDependencies, 'addEventListener' | 'removeEventListener'> = productionDependencies,
): () => void {
    const onDestination = (event: Event) => {
        const detail = (event as CustomEvent<AndroidSafDestinationEvent>).detail
        if (detail) listener(detail)
    }
    dependencies.addEventListener(DESTINATION_EVENT, onDestination)
    return () => dependencies.removeEventListener(DESTINATION_EVENT, onDestination)
}

export async function acknowledgeAndroidSafExport(
    requestId: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): Promise<boolean> {
    return await bridge.acknowledgeExport?.(requestId) === true
}

export async function markAndroidSafExportPublicationReady(
    requestId: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): Promise<boolean> {
    return await bridge.markExportPublicationReady?.(requestId) === true
}

export async function getAndroidSafExportStatus(
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): Promise<string | null> {
    return await bridge.getExportStatus?.() ?? null
}

export async function getAndroidSafExportSourceId(
    bridge: AndroidSafJavascriptBridge | undefined = (window as Window & {
        RisuSafBridge?: AndroidSafJavascriptBridge
    }).RisuSafBridge,
): Promise<string | null> {
    if (await bridge?.getExportStatus?.() != null) return null
    const exportId = await bridge?.getExportSourceId?.()
    return typeof exportId === 'string'
        && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(exportId)
        ? exportId
        : null
}

export async function discardAndroidSafSource(
    token: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): Promise<boolean> {
    return await bridge.discardSource?.(token) === true
}

async function recoverUnclaimedAndroidDestination(
    bridge: AndroidSafJavascriptBridge,
): Promise<void> {
    const encoded = await bridge.getExportStatus?.()
    if (!encoded || encoded.length > 8_192) return
    let terminal: Partial<AndroidSafDestinationEvent>
    try { terminal = JSON.parse(encoded) } catch { return }
    if (!terminal || typeof terminal !== 'object'
        || typeof terminal.requestId !== 'string'
        || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(terminal.requestId)
        || activeDestinationRequestIds.has(terminal.requestId)
        || !['download', 'legacyBackup', 'screenshot'].includes(terminal.sourceKind ?? '')
        || !['succeeded', 'failed', 'cancelled'].includes(terminal.state ?? '')
        || !Array.isArray(terminal.warningCodes)
        || terminal.warningCodes.some((warning) => typeof warning !== 'string')) return
    if (terminal.sourceKind === 'screenshot') {
        const { recoverAndroidScreenshotPublication } = await import('../nativeScreenshotArchiveWriter')
        const { invoke } = await import('@tauri-apps/api/core')
        await recoverAndroidScreenshotPublication({
            getStatus: () => encoded,
            invoke,
            acknowledgeAndroidSafExport: (id) => bridge.acknowledgeExport?.(id) ?? false,
        })
        return
    }
    // These publications have no deferred owner. Native acknowledgement still checks the exact receipt.
    if (!await bridge.acknowledgeExport?.(terminal.requestId)) {
        throw new AndroidSafDestinationError(terminal.requestId, 'acknowledgement-failed', terminal.warningCodes,
            'Android SAF terminal receipt could not be acknowledged')
    }
}

/** Release the reservation without replacing a failed operation's error. */
export async function withAndroidSafDestination<T>(
    reservation: Pick<AndroidSafDestinationReservation, 'release'> | undefined,
    operation: () => Promise<T>,
): Promise<T> {
    let failed = false
    let originalError: unknown
    try {
        return await operation()
    } catch (error) {
        failed = true
        originalError = error
        throw error
    } finally {
        try { await reservation?.release() }
        catch (cleanupError) {
            if (!failed) throw cleanupError
            // Cleanup must not replace the producer's error, including cancellation.
            if (originalError && typeof originalError === 'object' && cleanupError instanceof AndroidSafDestinationError) {
                try {
                    const previous = (originalError as {warningCodes?: unknown}).warningCodes
                    Object.assign(originalError, {warningCodes: [...new Set([
                        ...(Array.isArray(previous) ? previous : []), ...cleanupError.warningCodes,
                    ])]})
                } catch {}
            }
        }
    }
}

export function reserveAndroidSafDestination(
    suggestedName: string,
    sourceKind: AndroidSafExportKind,
    signal?: AbortSignal,
    dependencies: AndroidSafDestinationDependencies = productionDependencies,
): Promise<AndroidSafDestinationReservation> {
    if (signal?.aborted) return Promise.reject(new DOMException('Android SAF export was cancelled', 'AbortError'))
    const { bridge } = dependencies
    if (!bridge.reserveExport || !bridge.bindExport || !bridge.releaseExport || !bridge.copyReservedExport) {
        return Promise.reject(new Error('Android SAF destination reservation is unavailable'))
    }
    const requestId = dependencies.createRequestId()
    let jobId: string | undefined
    activeDestinationRequestIds.add(requestId)
    const reservedEvent = 'risunest-android-saf-reserved'
    return withLibraryFileSelection(() => new Promise((resolve, reject) => {
        let settled = false
        const cleanup = () => {
            signal?.removeEventListener('abort', onAbort)
            dependencies.removeEventListener(reservedEvent, onReserved)
            dependencies.removeEventListener(DESTINATION_EVENT, onTerminal)
        }
        const release = async () => {
            try {
                if (!await bridge.releaseExport!(requestId)) return
                // Copies retain their terminal receipt for their publication owner to acknowledge.
                const status = await bridge.getExportStatus?.()
                const terminal = status ? JSON.parse(status) as AndroidSafDestinationEvent : null
                if (terminal?.requestId === requestId) {
                    await bridge.acknowledgeExport?.(requestId)
                    if (terminal.warningCodes.includes('partial-destination-may-remain')) {
                        throw new AndroidSafDestinationError(requestId, 'destination-cleanup-failed', terminal.warningCodes,
                            'A partial file may remain at the Android SAF destination')
                    }
                }
            } finally {
                activeDestinationRequestIds.delete(requestId)
            }
        }
        const fail = (error: unknown) => {
            if (settled) return
            settled = true
            cleanup()
            void release().catch((cleanupError) => {
                if (error && typeof error === 'object' && cleanupError instanceof AndroidSafDestinationError) {
                    Object.assign(error, {warningCodes: cleanupError.warningCodes})
                }
            }).finally(() => reject(error))
        }
        const onAbort = () => {
            void Promise.resolve(bridge.cancelExport?.(requestId)).catch(() => {})
        }
        const onReserved = (event: Event) => {
            if ((event as CustomEvent<{requestId: string}>).detail?.requestId !== requestId || settled) return
            if (signal?.aborted) {
                fail(new DOMException('Android SAF export was cancelled', 'AbortError'))
                return
            }
            settled = true
            cleanup()
            resolve({
                requestId,
                get jobId() { return jobId },
                async bind(exportId) {
                    if (!await bridge.bindExport!(requestId, exportId)) {
                        throw new AndroidSafDestinationError(requestId, 'invalid-destination', [], 'Android SAF destination does not match the export')
                    }
                    jobId = exportId
                },
                release,
            })
        }
        const onTerminal = (event: Event) => {
            const detail = (event as CustomEvent<AndroidSafDestinationEvent>).detail
            if (detail?.requestId !== requestId) return
            fail(detail.state === 'cancelled' ? androidSafAbortError(requestId, detail)
                : new AndroidSafDestinationError(requestId, detail.code ?? 'destination-picker-failed', detail.warningCodes,
                    detail.message ?? 'Android SAF destination selection failed'))
        }
        dependencies.addEventListener(reservedEvent, onReserved)
        dependencies.addEventListener(DESTINATION_EVENT, onTerminal)
        signal?.addEventListener('abort', onAbort, {once: true})
        try {
            const reserve = () => {
                if (signal?.aborted) {
                    fail(new DOMException('Android SAF export was cancelled', 'AbortError'))
                    return
                }
                return bridge.reserveExport!(requestId, suggestedName, sourceKind)
            }
            void Promise.resolve(bridge.getExportStatus
                ? recoverUnclaimedAndroidDestination(bridge).then(reserve)
                : reserve()).catch(fail)
        } catch (error) { fail(error) }
    }))
}

export function copyNativeExportToAndroidSaf(
    request: AndroidSafDestinationRequest,
    dependencies: AndroidSafDestinationDependencies = productionDependencies,
): Promise<AndroidSafDestinationResult> {
    if (request.signal?.aborted) {
        return Promise.reject(new DOMException('Android SAF export was cancelled', 'AbortError'))
    }
    const requestId = request.reservation?.requestId ?? dependencies.createRequestId()
    activeDestinationRequestIds.add(requestId)
    return withLibraryFileSelection(() => new Promise((resolve, reject) => {
        let settled = false
        let receiving = false
        const cleanup = () => {
            request.signal?.removeEventListener('abort', onAbort)
            dependencies.removeEventListener(DESTINATION_EVENT, onEvent)
            queueMicrotask(() => activeDestinationRequestIds.delete(requestId))
            if (request.onProgress) {
                dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
            }
        }
        const finish = (callback: () => void) => {
            if (settled) return
            settled = true
            cleanup()
            callback()
        }
        const onAbort = () => {
            try {
                void Promise.resolve(dependencies.bridge.cancelExport?.(requestId)).catch(() => {})
            } catch {}
            // Keep the source owned until this copy reports its terminal outcome.
        }
        const onEvent = async (event: Event) => {
            const detail = (event as CustomEvent<AndroidSafDestinationEvent>).detail
            if (!detail || detail.requestId !== requestId || settled || receiving) return
            receiving = true
            if (!request.deferAcknowledgement && dependencies.bridge.acknowledgeExport) {
                const acknowledged = await Promise.resolve()
                    .then(() => dependencies.bridge.acknowledgeExport!(requestId))
                    .catch(() => false)
                if (!acknowledged && detail.state === 'succeeded') {
                    finish(() => reject(new AndroidSafDestinationError(
                        requestId, 'acknowledgement-failed', detail.warningCodes,
                        'Android SAF publication completed but its receipt could not be acknowledged',
                    )))
                    return
                }
            }
            if (detail.state === 'succeeded' && typeof detail.bytes === 'number') {
                finish(() => resolve({
                    requestId,
                    bytes: detail.bytes as number,
                    warningCodes: detail.warningCodes,
                }))
                return
            }
            if (detail.state === 'cancelled') {
                finish(() => reject(androidSafAbortError(requestId, detail)))
                return
            }
            finish(() => reject(new AndroidSafDestinationError(
                requestId,
                detail.code ?? 'destination-write-failed',
                detail.warningCodes,
                detail.message ?? 'Android SAF destination copy failed',
            )))
        }
        const onProgress = (event: Event) => {
            const detail = (event as CustomEvent<AndroidSafProgress>).detail
            if (
                detail?.requestId === requestId &&
                detail.operation === 'destination-copy'
            ) {
                request.onProgress?.(detail)
            }
        }
        dependencies.addEventListener(DESTINATION_EVENT, onEvent)
        if (request.onProgress) {
            dependencies.addEventListener(PROGRESS_EVENT, onProgress)
        }
        request.signal?.addEventListener('abort', onAbort, { once: true })
        try {
            const started = request.reservation
                ? Promise.resolve(request.reservation.jobId ? undefined : bindAndroidSafDestinationSource(request.reservation, request.sourcePath))
                    .then(() => {
                        if (!dependencies.bridge.copyReservedExport || !request.reservation?.jobId) throw new Error('Android SAF reservation is not bound')
                        return dependencies.bridge.copyReservedExport(requestId, request.sourcePath, request.reservation.jobId)
                    })
                : dependencies.bridge.copyExport(requestId, request.sourcePath, request.suggestedName)
            void Promise.resolve(started).catch((error) => finish(() => reject(error)))
        }
        catch (error) {
            finish(() => reject(error))
        }
    }))
}

export interface AndroidContentSources {
    sources: AndroidSpoolReady[]
    /** Picked files that cannot be imported; the picked files that can are still returned. */
    failures: AndroidSpoolFailure[]
}

export function pickAndroidContentSources(
    options: AndroidSafContentSourcePickerOptions,
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<AndroidContentSources | null> {
    const config: AndroidSafSourcePickerConfig = {
        eventName: 'risu-android-content-source-picked',
        pickMethod: 'pickContentSource',
        pickerUnavailableMessage: 'Android content picker is unavailable',
        acceptExtension: [
            '.json',
            '.png',
            '.charx',
            '.jpg',
            '.jpeg',
            '.risum',
            '.lorebook',
        ],
        cancelMessage: 'Android content selection was cancelled',
        importDestination: options.destination,
    }
    return receiveAndroidSpoolBatch(config, options, dependencies, async (batch, discard) => {
        const sources: AndroidSpoolReady[] = []
        const failures = [...batch.failures]
        for (const source of batch.ready) {
            if (acceptsSpool(config, source)) {
                sources.push(source)
                continue
            }
            await Promise.resolve().then(() => discard(source)).catch(() => false)
            failures.push({ displayName: source.displayName, code: 'unsupported-format' })
        }
        // A single refused file keeps the single-file failure; several are listed together.
        if (sources.length || failures.length > 1) return { sources, failures }
        if (!failures.length) return null
        throw new AndroidSafSourceError(failures[0].code, `${failures[0].displayName}: ${failures[0].code}`)
    })
}
