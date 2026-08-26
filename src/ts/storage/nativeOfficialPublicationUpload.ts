export type NativeOfficialPublicationCredential =
    | { kind: 'risu-auth'; token: string }
    | { kind: 'bearer'; token: string }

export interface NativeOfficialPublicationFile {
    path: string
    bytes: number
}

interface NativeOfficialPublicationCommonResult {
    session: string
    saveDate: string
    status: number
    bytesUploaded: number
}

type NativeOfficialPublicationAttemptResult =
    | (NativeOfficialPublicationCommonResult & {
          kind: 'written'
          replacementKey: string
          warning: string | null
          reloadSession: boolean
      })
    | (NativeOfficialPublicationCommonResult & {
          kind: 'not-modified'
          replacementKey: string
      })
    | (NativeOfficialPublicationCommonResult & { kind: 'auth-warning' })
    | (NativeOfficialPublicationCommonResult & { kind: 'reauthentication-needed' })

export type NativeOfficialPublicationResult =
    | {
          kind: 'written'
          replacementKey: string
          status: number
          bytesUploaded: number
          warning: string | null
          reloadSession: boolean
      }
    | {
          kind: 'not-modified'
          replacementKey: string
          status: number
          bytesUploaded: number
      }
    | { kind: 'auth-warning'; status: number; bytesUploaded: number }

export interface NativeOfficialPublicationFileUploaderDependencies {
    baseUrl: string
    credential(): NativeOfficialPublicationCredential | null
    invoke(command: string, args: Record<string, unknown>): Promise<unknown>
    now(): number
    reauthenticate(): Promise<void>
}

function throwIfAborted(signal?: AbortSignal): void {
    if (!signal?.aborted) return
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

function assertAttemptResult(value: unknown): NativeOfficialPublicationAttemptResult {
    if (!value || typeof value !== 'object') {
        throw new Error('Native official publication returned an invalid result')
    }
    const result = value as Partial<NativeOfficialPublicationAttemptResult>
    if (
        typeof result.kind !== 'string'
        || typeof result.session !== 'string'
        || typeof result.saveDate !== 'string'
        || typeof result.status !== 'number'
        || typeof result.bytesUploaded !== 'number'
    ) {
        throw new Error('Native official publication returned an invalid result')
    }
    return result as NativeOfficialPublicationAttemptResult
}

export class NativeOfficialPublicationFileUploader {
    private session: string | null = null

    constructor(
        private readonly dependencies: NativeOfficialPublicationFileUploaderDependencies,
    ) {}

    async upload(
        file: NativeOfficialPublicationFile,
        signal?: AbortSignal,
    ): Promise<NativeOfficialPublicationResult> {
        while (true) {
            throwIfAborted(signal)
            const credential = this.dependencies.credential()
            if (!credential?.token) {
                throw new Error('Official account credential is unavailable')
            }
            const result = assertAttemptResult(await this.dependencies.invoke(
                'official_publication_upload_file',
                {
                    request: {
                        baseUrl: this.dependencies.baseUrl,
                        credential,
                        path: file.path,
                        saveDate: this.dependencies.now().toFixed(0),
                        session: this.session,
                    },
                },
            ))
            throwIfAborted(signal)
            this.session = result.session
            if (result.bytesUploaded !== file.bytes) {
                throw new Error(
                    `Native official publication uploaded ${result.bytesUploaded} bytes, expected ${file.bytes}`,
                )
            }
            if (result.kind === 'reauthentication-needed') {
                await this.dependencies.reauthenticate()
                continue
            }
            if (result.kind === 'written') {
                return {
                    kind: result.kind,
                    replacementKey: result.replacementKey,
                    status: result.status,
                    bytesUploaded: result.bytesUploaded,
                    warning: result.warning,
                    reloadSession: result.reloadSession,
                }
            }
            if (result.kind === 'not-modified') {
                return {
                    kind: result.kind,
                    replacementKey: result.replacementKey,
                    status: result.status,
                    bytesUploaded: result.bytesUploaded,
                }
            }
            return {
                kind: result.kind,
                status: result.status,
                bytesUploaded: result.bytesUploaded,
            }
        }
    }
}
