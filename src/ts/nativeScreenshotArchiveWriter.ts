import { invoke } from '@tauri-apps/api/core'
import { save } from '@tauri-apps/plugin-dialog'
import type { ScreenshotArchiveWriter } from './chatScreenshotArchive'
import {
    acknowledgeAndroidSafExport,
    copyNativeExportToAndroidSaf,
    getAndroidSafExportStatus,
    isAndroidSafDestinationRequestActive,
    listenAndroidSafDestinationEvents,
    type AndroidSafDestinationEvent,
    type AndroidSafDestinationRequest,
    type AndroidSafDestinationResult,
} from './storage/androidSafBridge'
import { listenRecoveredPublications } from './storage/recoveredPublicationListener'
import { exportIOSFile } from './storage/iosFiles'

export const SCREENSHOT_OUTPUT_CHUNK_BYTES = 64 * 1024

interface NativeScreenshotArchiveDependencies {
    selectDestination(defaultName: string): Promise<string | null>
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    warn(warningCode: string): void
}

interface AndroidScreenshotArchiveDependencies {
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    copyToAndroidSaf(request: AndroidSafDestinationRequest): Promise<AndroidSafDestinationResult>
    acknowledgeAndroidSafExport(requestId: string): boolean | Promise<boolean>
    warn(warningCode: string): void
}

interface MobileScreenshotArchiveDependencies {
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    exportFile(request: {
        sourcePath: string
        suggestedName: string
        signal?: AbortSignal
    }): Promise<{ bytes: number; requestId?: string; warningCodes?: string[] }>
    acknowledgePublication?(requestId: string): boolean | Promise<boolean>
    warn(warningCode: string): void
}

const productionIOSDependencies: MobileScreenshotArchiveDependencies = {
    invoke: (command, args) => invoke(command, args),
    exportFile: exportIOSFile,
    warn: (warningCode) => console.warn(`iOS screenshot output warning: ${warningCode}`),
}

const productionDependencies: NativeScreenshotArchiveDependencies = {
    selectDestination: (defaultName) => save({
        defaultPath: defaultName,
        filters: [{ name: 'ZIP', extensions: ['zip'] }],
    }),
    invoke: (command, args) => invoke(command, args),
    warn: (warningCode) => console.warn(`Native screenshot output warning: ${warningCode}`),
}

const productionAndroidDependencies: AndroidScreenshotArchiveDependencies = {
    invoke: (command, args) => invoke(command, args),
    copyToAndroidSaf: (request) => copyNativeExportToAndroidSaf(request),
    acknowledgeAndroidSafExport: (requestId) => acknowledgeAndroidSafExport(requestId),
    warn: (warningCode) => console.warn(`Android screenshot output warning: ${warningCode}`),
}

function warningCodes(result: unknown): string[] {
    if (!result || typeof result !== 'object' || !('warningCodes' in result)) return []
    const warnings = (result as { warningCodes?: unknown }).warningCodes
    if (!Array.isArray(warnings)) return []
    return [...new Set(warnings.filter((warning): warning is string => typeof warning === 'string'))]
}

function readMobileHandoff(result: unknown): { bytes: number; sourcePath: string } {
    if (!result || typeof result !== 'object') {
        throw new Error('Native screenshot output returned an invalid mobile handoff')
    }
    const value = result as { bytes?: unknown; sourcePath?: unknown }
    if (
        typeof value.bytes !== 'number'
        || !Number.isSafeInteger(value.bytes)
        || value.bytes < 0
        || typeof value.sourcePath !== 'string'
        || !value.sourcePath
    ) {
        throw new Error('Native screenshot output returned an invalid mobile handoff')
    }
    return { bytes: value.bytes, sourcePath: value.sourcePath }
}

function screenshotPublicationError(code: string, message: string, warningCodes: string[] = []) {
    return Object.assign(new Error(message), { code, warningCodes })
}

async function appendNativeChunk(
    jobId: string,
    chunk: Uint8Array,
    invokeCommand: AndroidScreenshotArchiveDependencies['invoke'],
) {
    for (let offset = 0; offset < chunk.byteLength; offset += SCREENSHOT_OUTPUT_CHUNK_BYTES) {
        const part = chunk.subarray(
            offset,
            Math.min(offset + SCREENSHOT_OUTPUT_CHUNK_BYTES, chunk.byteLength),
        )
        await invokeCommand('native_file_job_screenshot_output_append', {
            jobId,
            chunk: Array.from(part),
        })
    }
}

class NativeScreenshotArchiveWriter implements ScreenshotArchiveWriter {
    private state: 'open' | 'publishing' | 'closed' | 'aborted' = 'open'
    private publication: Promise<void> | null = null
    private aborting: Promise<boolean> | null = null

    constructor(
        private readonly jobId: string,
        private readonly dependencies: NativeScreenshotArchiveDependencies,
    ) {}

    async write(chunk: Uint8Array): Promise<void> {
        if (this.state === 'aborted') throw new Error('Native screenshot output was aborted')
        if (this.state !== 'open') throw new Error('Native screenshot output is finalized')
        await appendNativeChunk(this.jobId, chunk, this.dependencies.invoke)
    }

    async close(): Promise<void> {
        if (this.state === 'aborted') throw new Error('Native screenshot output was aborted')
        if (this.state === 'closed') return
        if (this.state !== 'open') throw new Error('Native screenshot output is being published')
        this.state = 'publishing'
        const publication = this.dependencies.invoke(
            'native_file_job_screenshot_output_publish',
            { jobId: this.jobId },
        ).then((result) => {
            for (const warningCode of warningCodes(result)) {
                this.dependencies.warn(warningCode)
            }
        })
        this.publication = publication
        try {
            await publication
            if (!this.isAborted()) this.state = 'closed'
        } catch (error) {
            if (!this.isAborted()) this.state = 'open'
            throw error
        }
    }

    abort(): Promise<boolean> {
        if (this.aborting) return this.aborting
        this.aborting = this.abortOutput()
        return this.aborting
    }

    private isAborted() {
        return this.state === 'aborted'
    }

    private async abortOutput(): Promise<boolean> {
        if (this.state === 'aborted') return true
        if (this.state === 'closed') return false
        const publication = this.publication
        const outcome = await this.dependencies.invoke(
            'native_file_job_screenshot_output_cancel',
            { jobId: this.jobId },
        )
        if (publication && (outcome === 'tooLate' || outcome === 'missing')) {
            try {
                await publication
                this.state = 'closed'
                return false
            } catch {
                this.state = 'aborted'
                return true
            }
        }
        this.state = 'aborted'
        if (publication) {
            try {
                await publication
            } catch {
                // The native cancellation path reports completion by rejecting publication.
            }
        }
        return true
    }
}

class MobileScreenshotArchiveWriter implements ScreenshotArchiveWriter {
    private state: 'open' | 'publishing' | 'closed' | 'aborted' = 'open'
    private publication: Promise<void> | null = null
    private aborting: Promise<boolean> | null = null
    private releasing: Promise<void> | null = null
    private readonly publicationController = new AbortController()
    private destinationCommitted = false
    private destinationRequestId: string | null = null
    private handoffReady = false
    private publicationError: unknown

    constructor(
        private readonly jobId: string,
        private readonly suggestedName: string,
        private readonly dependencies: MobileScreenshotArchiveDependencies,
    ) {}

    async write(chunk: Uint8Array): Promise<void> {
        if (this.state === 'aborted') throw new Error('Mobile screenshot output was aborted')
        if (this.state !== 'open') throw new Error('Mobile screenshot output is finalized')
        await appendNativeChunk(this.jobId, chunk, this.dependencies.invoke)
    }

    async close(): Promise<void> {
        if (this.state === 'aborted') throw new Error('Mobile screenshot output was aborted')
        if (this.state === 'closed') return
        if (this.state !== 'open') throw new Error('Mobile screenshot output is being published')
        this.state = 'publishing'
        this.publication = this.publish().catch((error) => {
            this.publicationError = error
            throw error
        })
        try {
            await this.publication
            this.state = 'closed'
        }
        catch (error) {
            this.state = 'aborted'
            throw error
        }
    }

    abort(): Promise<boolean> {
        if (this.aborting) return this.aborting
        this.aborting = this.abortOutput()
        return this.aborting
    }

    private async publish() {
        try {
            const prepared = await this.dependencies.invoke(
                'native_file_job_screenshot_output_publish',
                { jobId: this.jobId },
            )
            const handoff = readMobileHandoff(prepared)
            this.handoffReady = true
            for (const warningCode of warningCodes(prepared)) this.dependencies.warn(warningCode)
            const published = await this.dependencies.exportFile({
                sourcePath: handoff.sourcePath,
                suggestedName: this.suggestedName,
                signal: this.publicationController.signal,
            })
            this.destinationRequestId = published.requestId ?? null
            this.destinationCommitted = true
            for (const warningCode of warningCodes(published)) this.dependencies.warn(warningCode)
            if (published.bytes !== handoff.bytes) {
                throw screenshotPublicationError(
                    'length-mismatch',
                    'Mobile screenshot output length does not match its native handoff',
                    ['partial-destination-may-remain'],
                )
            }
        }
        catch (error) {
            this.destinationRequestId = readAndroidSafRequestId(error) ?? this.destinationRequestId
            throw error
        }
        finally {
            try {
                await this.release()
            }
            catch {
                this.dependencies.warn('cleanup-failed')
            }
            if (
                this.destinationRequestId
                && this.dependencies.acknowledgePublication
                && !await this.dependencies.acknowledgePublication(this.destinationRequestId)
            ) {
                this.dependencies.warn('cleanup-failed')
            }
        }
    }

    private release() {
        if (!this.releasing) {
            this.releasing = this.dependencies.invoke(
                'native_file_job_screenshot_output_release',
                { jobId: this.jobId },
            ).then(
                () => undefined,
                (error) => {
                    this.releasing = null
                    throw error
                },
            )
        }
        return this.releasing
    }

    private async abortOutput(): Promise<boolean> {
        if (this.state === 'aborted') {
            try {
                await this.release()
            } catch {
                this.dependencies.warn('cleanup-failed')
            }
            if (this.publicationError) throw this.publicationError
            return true
        }
        if (this.state === 'closed') return false
        const nativeCancellation = this.handoffReady
            ? null
            : this.dependencies.invoke(
                'native_file_job_screenshot_output_cancel',
                { jobId: this.jobId },
            )
        this.publicationController.abort()
        if (this.publication) {
            try {
                await this.publication
            }
            catch {
                // The SAF path reports cancellation by rejecting publication.
            }
            await nativeCancellation
            if (this.publicationError) {
                this.state = 'aborted'
                throw this.publicationError
            }
            if (this.destinationCommitted) {
                this.state = 'closed'
                return false
            }
            this.state = 'aborted'
            return true
        }
        await nativeCancellation
        this.state = 'aborted'
        return true
    }
}

function readAndroidSafRequestId(error: unknown): string | null {
    if (!error || typeof error !== 'object' || !('requestId' in error)) return null
    const requestId = (error as { requestId?: unknown }).requestId
    return typeof requestId === 'string' ? requestId : null
}

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

function recoveredScreenshotTerminal(encoded: string | null): AndroidSafDestinationEvent | null {
    if (!encoded || encoded.length > 8_192) return null
    let value: unknown
    try {
        value = JSON.parse(encoded)
    }
    catch {
        return null
    }
    if (!value || typeof value !== 'object') return null
    const event = value as Partial<AndroidSafDestinationEvent>
    if (
        typeof event.requestId !== 'string'
        || !UUID_V4.test(event.requestId)
        || typeof event.exportId !== 'string'
        || !UUID_V4.test(event.exportId)
        || event.sourceKind !== 'screenshot'
        || !['succeeded', 'failed', 'cancelled'].includes(event.state ?? '')
        || !Array.isArray(event.warningCodes)
        || event.warningCodes.some((warning) => typeof warning !== 'string')
    ) return null
    return event as AndroidSafDestinationEvent
}

interface AndroidScreenshotRecoveryDependencies {
    getStatus(): string | null | Promise<string | null>
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    acknowledgeAndroidSafExport(requestId: string): boolean | Promise<boolean>
}

const productionAndroidRecoveryDependencies: AndroidScreenshotRecoveryDependencies = {
    getStatus: () => getAndroidSafExportStatus(),
    invoke: (command, args) => invoke(command, args),
    acknowledgeAndroidSafExport: (requestId) => acknowledgeAndroidSafExport(requestId),
}

export async function recoverAndroidScreenshotPublication(
    dependencies: AndroidScreenshotRecoveryDependencies = productionAndroidRecoveryDependencies,
): Promise<AndroidSafDestinationEvent | null> {
    const terminal = recoveredScreenshotTerminal(await dependencies.getStatus())
    if (!terminal || !terminal.exportId) return null
    await dependencies.invoke('native_file_job_screenshot_output_release', {
        jobId: terminal.exportId,
    })
    if (!await dependencies.acknowledgeAndroidSafExport(terminal.requestId)) {
        throw new Error('Android screenshot destination journal could not be acknowledged')
    }
    return terminal
}

interface AndroidScreenshotRecoveryListenerDependencies
    extends AndroidScreenshotRecoveryDependencies {
    listen(listener: (event: AndroidSafDestinationEvent) => void): () => void
    isActive(requestId: string): boolean
}

const productionAndroidRecoveryListenerDependencies: AndroidScreenshotRecoveryListenerDependencies = {
    ...productionAndroidRecoveryDependencies,
    listen: (listener) => listenAndroidSafDestinationEvents(listener),
    isActive: (requestId) => isAndroidSafDestinationRequestActive(requestId),
}

export function listenRecoveredAndroidScreenshotPublications(
    onTerminal: (terminal: AndroidSafDestinationEvent) => void,
    onError: (error: unknown) => void,
    dependencies: AndroidScreenshotRecoveryListenerDependencies = productionAndroidRecoveryListenerDependencies,
): () => void {
    return listenRecoveredPublications({
        sourceKind: 'screenshot',
        onTerminal,
        onError,
        listen: dependencies.listen,
        isActive: dependencies.isActive,
        recoverEvent: (event) => recoverAndroidScreenshotPublication({
            ...dependencies,
            getStatus: () => JSON.stringify(event),
        }),
        initial: {
            recover: async () => {
                const encoded = await dependencies.getStatus()
                const terminal = recoveredScreenshotTerminal(encoded)
                if (!terminal || dependencies.isActive(terminal.requestId)) return null
                return recoverAndroidScreenshotPublication({
                    ...dependencies,
                    getStatus: () => encoded,
                })
            },
        },
    })
}

export async function createNativeScreenshotArchiveWriter(
    defaultName: string,
    dependencies: NativeScreenshotArchiveDependencies = productionDependencies,
): Promise<ScreenshotArchiveWriter> {
    const destination = await dependencies.selectDestination(defaultName)
    if (!destination) {
        throw new DOMException('Screenshot export was cancelled', 'AbortError')
    }
    const started = await dependencies.invoke('native_file_job_screenshot_output_start', {
        destination,
    }) as { jobId?: unknown }
    if (typeof started.jobId !== 'string' || !started.jobId) {
        throw new Error('Native screenshot output returned an invalid job ID')
    }
    return new NativeScreenshotArchiveWriter(started.jobId, dependencies)
}

export function createAndroidScreenshotArchiveWriter(
    suggestedName: string,
    dependencies: AndroidScreenshotArchiveDependencies = productionAndroidDependencies,
): Promise<ScreenshotArchiveWriter> {
    return createMobileScreenshotArchiveWriter(suggestedName, {
        invoke: dependencies.invoke,
        exportFile: (request) => dependencies.copyToAndroidSaf({ ...request, deferAcknowledgement: true }),
        acknowledgePublication: dependencies.acknowledgeAndroidSafExport,
        warn: dependencies.warn,
    })
}

export function createIOSScreenshotArchiveWriter(
    suggestedName: string,
    dependencies: MobileScreenshotArchiveDependencies = productionIOSDependencies,
): Promise<ScreenshotArchiveWriter> {
    return createMobileScreenshotArchiveWriter(suggestedName, dependencies)
}

async function createMobileScreenshotArchiveWriter(
    suggestedName: string,
    dependencies: MobileScreenshotArchiveDependencies,
): Promise<ScreenshotArchiveWriter> {
    const started = await dependencies.invoke('native_file_job_screenshot_output_start', {
        destination: null,
    }) as { jobId?: unknown }
    if (typeof started.jobId !== 'string' || !started.jobId) {
        throw new Error('Native screenshot output returned an invalid job ID')
    }
    return new MobileScreenshotArchiveWriter(started.jobId, suggestedName, dependencies)
}

export function describeScreenshotPublicationError(error: unknown, partialWarning: string) {
    const detail = error instanceof Error ? error.message : String(error)
    if (
        error
        && typeof error === 'object'
        && 'warningCodes' in error
        && Array.isArray((error as { warningCodes?: unknown }).warningCodes)
        && (error as { warningCodes: unknown[] }).warningCodes
            .includes('partial-destination-may-remain')
    ) {
        return `${detail} ${partialWarning}`
    }
    return detail
}
