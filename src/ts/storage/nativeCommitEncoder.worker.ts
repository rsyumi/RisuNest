import type { CommitEnvelope } from './nativeCommitTransport'

self.onmessage = ({ data }: MessageEvent<CommitEnvelope>) => {
    try {
        const bytes = new TextEncoder().encode(JSON.stringify(data))
        self.postMessage({ bytes }, { transfer: [bytes.buffer] })
    } catch (error) {
        self.postMessage({
            error: error instanceof Error ? error.message : 'Persistence encoding failed',
        })
    }
}
