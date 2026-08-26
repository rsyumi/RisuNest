import { invoke } from '@tauri-apps/api/core'
import { save } from '@tauri-apps/plugin-dialog'
import type { ScreenshotArchiveWriter } from './chatScreenshotArchive'

export const SCREENSHOT_OUTPUT_CHUNK_BYTES = 64 * 1024

interface NativeScreenshotArchiveDependencies {
    selectDestination(defaultName: string): Promise<string | null>
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    warn(warningCode: string): void
}

const productionDependencies: NativeScreenshotArchiveDependencies = {
    selectDestination: (defaultName) => save({
        defaultPath: defaultName,
        filters: [{ name: 'ZIP', extensions: ['zip'] }],
    }),
    invoke: (command, args) => invoke(command, args),
    warn: (warningCode) => console.warn(`Native screenshot output warning: ${warningCode}`),
}

function warningCodes(result: unknown): string[] {
    if (!result || typeof result !== 'object' || !('warningCodes' in result)) return []
    const warnings = (result as { warningCodes?: unknown }).warningCodes
    if (!Array.isArray(warnings)) return []
    return [...new Set(warnings.filter((warning): warning is string => typeof warning === 'string'))]
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
        for (let offset = 0; offset < chunk.byteLength; offset += SCREENSHOT_OUTPUT_CHUNK_BYTES) {
            const part = chunk.subarray(
                offset,
                Math.min(offset + SCREENSHOT_OUTPUT_CHUNK_BYTES, chunk.byteLength),
            )
            await this.dependencies.invoke('native_file_job_screenshot_output_append', {
                jobId: this.jobId,
                chunk: Array.from(part),
            })
        }
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
