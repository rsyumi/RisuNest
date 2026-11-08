import { invoke } from '@tauri-apps/api/core'
import { saveNativeExportFile, type NativeExportResult } from '../storage/nativeExportFile'

export interface NetworkLogSettings {
    enabled: boolean
    retain: boolean
    hideAuth: boolean
    recentCount: number
    recentMib: number
    retainedMib: number
    bufferMib: number
    captureBinary: boolean
}
export type LogSide = 'request' | 'response'
export interface LogHeader { name: string; value: string; encoding: string | null }
export interface LogBody { observedBytes: number; capturedBytes: number; binary: boolean; truncated: boolean; representation: string }
export interface LogMessage { method: string | null; url: string; status: number | null; headers: LogHeader[]; body: LogBody }
export interface NetworkLogRecord {
    operationId?: string | null
    id: string; source: string; startedAt: number; elapsedMs: number; retained: boolean; headerMasking: boolean
    state: string; captureError: string | null; request: LogMessage; response: LogMessage
    streamKind: 'sse' | 'websocket' | null; eventBytes: number
}
export interface NetworkLogItem {
    operationId?: string | null
    id: string; source: string; startedAt: number; elapsedMs: number; retained: boolean; method: string; url: string
    status: number | null; state: string; capturedBytes: number; incomplete: boolean; streamKind: string | null
}
export interface NetworkLogListing { items: NetworkLogItem[]; hasMore: boolean; recentBytes: number; retainedBytes: number; warning: string | null }

export function groupNetworkLogItems(items: readonly NetworkLogItem[]): NetworkLogItem[][] {
    const groups = new Map<string, NetworkLogItem[]>()
    for (const item of items) {
        const key = item.operationId ? `operation:${item.operationId}` : `record:${item.id}`
        const group = groups.get(key)
        if (group) group.push(item)
        else groups.set(key, [item])
    }
    return [...groups.values()]
}
export interface LogBodyPage { text: string; offset: number; nextOffset: number; total: number; binary: boolean }
export interface LogObservation { elapsedMs: number; side: LogSide; offset: number; length: number; kind: string }
/** `next` is the position the following SSE page passes back as `from`. */
export interface LogArrivals { times: (number | null)[]; next: number }
export type LogOperation =
    | { op: 'settings' }
    | { op: 'configure'; settings: NetworkLogSettings }
    | { op: 'list'; query: { retained: boolean; search: string; status: string; offset: number } }
    | { op: 'detail' | 'retain' | 'delete'; id: string }
    | { op: 'body'; id: string; side: LogSide; offset: number; length?: number; hex?: boolean }
    | { op: 'events'; id: string; offset: number }
    | { op: 'arrivalTimes'; id: string; ends: number[]; from: number }
    | { op: 'copy'; id: string; side: LogSide }
    | { op: 'export'; id: string; destination?: string }
export function networkLog<T>(request: LogOperation): Promise<T> { return invoke<T>('network_log_command', { request }) }

export function logSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`
    const units = ['KiB', 'MiB', 'GiB']
    let value = bytes / 1024, index = 0
    while (value >= 1024 && index < units.length - 1) { value /= 1024; index++ }
    return `${value.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${units[index]}`
}
export function downloadNetworkLog(id: string): Promise<boolean> {
    return saveNativeExportFile(`network-${id}.json`, [{ name: 'JSON', extensions: ['json'] }],
        destination => networkLog<NativeExportResult>({ op: 'export', id, destination }))
}

export function prettyLogJson(text: string): string | null {
    if (text.length > 64 * 1024) return null
    let depth = 0, quoted = false, escaped = false
    for (const char of text) {
        if (quoted) {
            if (escaped) escaped = false
            else if (char === '\\') escaped = true
            else if (char === '"') quoted = false
        } else if (char === '"') quoted = true
        else if (char === '[' || char === '{') { if (++depth > 24) return null }
        else if (char === ']' || char === '}') depth--
    }
    try { const value = JSON.stringify(JSON.parse(text), null, 2); return value.length <= 256 * 1024 ? value : null } catch { return null }
}

export interface SsePreviewEvent { event: string; id: string; data: string; raw: string; complete: boolean; partial: boolean; endOffset: number }
/** A bounded diagnostic view. The stored response remains the unmodified source. */
export function parseSsePreview(text: string, offset = 0, partialStart = offset > 0): SsePreviewEvent[] {
    const events: SsePreviewEvent[] = []
    let event = '', id = '', data: string[] = [], raw = '', first = true, bytes = offset
    const encoder = new TextEncoder()
    const emit = (complete: boolean) => {
        if (raw) events.push({ event: event || 'message', id, data: data.join('\n'), raw, complete, partial: first && partialStart, endOffset: bytes })
        first = false; event = ''; id = ''; data = []; raw = ''
    }
    const lines = text.matchAll(/([^\r\n]*)(\r\n|\r|\n|$)/g)
    for (const match of lines) {
        const line = match[1], ending = match[2]
        if (!line && !ending) break
        bytes += encoder.encode(line + ending).length
        if (!line && ending) { if (raw) raw += ending; emit(true); if (events.length >= 50) break; continue }
        raw += line + ending
        if (!ending) continue
        if (line.startsWith(':')) continue
        const colon = line.indexOf(':')
        const field = colon < 0 ? line : line.slice(0, colon)
        let value = colon < 0 ? '' : line.slice(colon + 1)
        if (value.startsWith(' ')) value = value.slice(1)
        if (field === 'event') event = value
        if (field === 'id' && !value.includes('\0')) id = value
        if (field === 'data') data.push(value)
    }
    emit(false)
    return events
}
