import type {
    RegexWorkerRequest,
    RegexWorkerResponse,
} from './regexWorkerClient'

interface CompiledEntry {
    sourceIndex: number
    pattern: string
    replacement: string
    regex?: RegExp
    compileError?: string
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error)
}

export function createRegexWorkerMessageHandler(
    respond: (response: RegexWorkerResponse) => void,
): (request: RegexWorkerRequest) => void {
    let registeredRevision: number | undefined
    let registeredEntries: CompiledEntry[] | undefined

    return (request) => {
        if (request.type === 'register') {
            registeredRevision = request.revision
            registeredEntries = request.entries.map(([
                sourceIndex,
                pattern,
                replacement,
                flags,
            ]) => {
                const entry: CompiledEntry = { sourceIndex, pattern, replacement }
                if (pattern !== '') {
                    try {
                        entry.regex = new RegExp(pattern, flags)
                    }
                    catch (error) {
                        entry.compileError = errorMessage(error)
                    }
                }
                return entry
            })
            return
        }

        if (registeredRevision !== request.revision || registeredEntries === undefined) {
            respond({
                type: 'error',
                id: request.id,
                message: `Regex plan revision ${request.revision} is not registered`,
            })
            return
        }

        let data = request.input
        const errors: [sourceIndex: number, message: string][] = []
        for (const entry of registeredEntries) {
            if (entry.pattern === '') {
                continue
            }
            try {
                if (entry.compileError !== undefined) {
                    throw new Error(entry.compileError)
                }
                if (entry.regex === undefined) {
                    throw new Error('Regex Worker entry was not compiled')
                }
                entry.regex.lastIndex = 0
                data = data.replace(entry.regex, entry.replacement)
            }
            catch (error) {
                errors.push([entry.sourceIndex, errorMessage(error)])
            }
        }

        respond({ type: 'result', id: request.id, data, errors })
    }
}

const workerScope = globalThis as typeof globalThis & {
    document?: unknown
    postMessage?: (message: RegexWorkerResponse) => void
    addEventListener?: (type: 'message', listener: EventListener) => void
}

if (workerScope.document === undefined
    && typeof workerScope.postMessage === 'function'
    && typeof workerScope.addEventListener === 'function') {
    const handleRequest = createRegexWorkerMessageHandler((response) => {
        workerScope.postMessage!(response)
    })
    workerScope.addEventListener('message', ((event: MessageEvent<RegexWorkerRequest>) => {
        handleRequest(event.data)
    }) as EventListener)
}
