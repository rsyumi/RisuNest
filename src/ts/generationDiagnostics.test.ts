import { describe, expect, it, vi } from 'vitest'
import { createGenerationDiagnostics, type GenerationDiagnosticAdmission, type GenerationListenerPayloadDiagnostic, type SyncRunDiagnostic } from './generationDiagnostics'

describe('generation diagnostic boundary', () => {
    it('drops early events and correlates later events with the native document and generation', async () => {
        const events: Record<string, unknown>[] = []
        let ready!: (value: number) => void
        const diagnostics = createGenerationDiagnostics({
            enabled: true,
            invoke: (_command, args) => {
                const event = args.event as Record<string, unknown>
                events.push(event)
                if (event.kind === 'document-start') return new Promise<number>((resolve) => { ready = resolve })
                return Promise.resolve(7)
            },
        })
        diagnostics.startDocument()
        diagnostics.startDocument()
        const first = diagnostics.generation()
        first.phase('send-admitted', 'enter')
        expect(events).toEqual([{ kind: 'document-start' }])
        ready(7)
        await Promise.resolve()
        await Promise.resolve()
        first.phase('host-response', 'exit', { resultKind: 'buffered', outcome: 'ok' })
        diagnostics.generation().phase('send-admitted', 'enter')
        diagnostics.lifecycle('memory-warning')
        expect(events.slice(1)).toEqual([
            expect.objectContaining({
                kind: 'phase', documentSequence: 7, generationSequence: 1, eventSequence: 1,
                phase: 'host-response', edge: 'exit', resultKind: 'buffered', outcome: 'ok',
            }),
            expect.objectContaining({ documentSequence: 7, generationSequence: 2, eventSequence: 2, phase: 'send-admitted' }),
            { kind: 'lifecycle', documentSequence: 7, eventSequence: 3, event: 'memory-warning' },
        ])
    })

    it('forwards only the structural event fields', async () => {
        const events: Record<string, unknown>[] = []
        const diagnostics = createGenerationDiagnostics({
            enabled: true,
            invoke: async (_command, args) => { events.push(args.event as Record<string, unknown>); return 3 },
        })
        diagnostics.startDocument()
        await Promise.resolve()
        const trace = diagnostics.generation()
        const admission = {
            intent: 'normal', displayMode: 'normal', profile: 'normal',
            historyLimitEnabled: true, historyLimitMultiplier: 1.5, route: 'history-window',
            prompt: 'PRIVATE_CONTENT_CANARY', documentSequence: 999,
        } satisfies GenerationDiagnosticAdmission & { prompt: string; documentSequence: number }
        trace.admission(admission)
        trace.persistence({
            boundary: 'checkpoint', savePath: 'windowed', pendingEvents: 4,
            dirtyRecords: 4, unacknowledgedMs: 2000, expectedRevision: 5, acknowledgedRevision: 6,
        })
        trace.listenerPayload({
            listeners: 2, fullHistoryListeners: 1, conversations: 3, messages: 40, messageCharacters: 900,
            characterName: 'PRIVATE_CONTENT_CANARY',
        } as GenerationListenerPayloadDiagnostic)
        diagnostics.syncRun({
            operation: 'receive', count: 2, more: true, outcome: 'complete', durationMs: 1234.6, connectionId: 'PRIVATE_CONTENT_CANARY',
        } as SyncRunDiagnostic)
        expect(JSON.stringify(events)).not.toContain('PRIVATE_CONTENT_CANARY')
        expect(events[1]).toMatchObject({ documentSequence: 3, generationSequence: 1, historyLimitMultiplier: 1.5 })
        expect(events[2]).toMatchObject({ expectedRevision: 5, acknowledgedRevision: 6 })
        // The native parser rejects unknown fields, so the emitted names are exact.
        expect(events[3]).toEqual({
            kind: 'listener-payload', documentSequence: 3, eventSequence: 3, generationSequence: 1,
            listeners: 2, fullHistoryListeners: 1, conversations: 3, messages: 40, messageCharacters: 900,
        })
        expect(events[4]).toEqual({
            kind: 'sync-run', documentSequence: 3, eventSequence: 4, target: 'external',
            operation: 'receive', count: 2, more: true, outcome: 'complete', durationMs: 1235,
        })
    })

    it('recovers when startup readiness precedes the first native document', async () => {
        let nativeSequence = 0
        const accepted: Record<string, unknown>[] = []
        const diagnostics = createGenerationDiagnostics({
            enabled: true,
            invoke: async (_command, args) => {
                const event = args.event as Record<string, unknown>
                if (event.kind === 'document-start') return nativeSequence
                if (!nativeSequence || event.documentSequence !== nativeSequence) throw 'stale-diagnostic-document'
                accepted.push(event)
                return nativeSequence
            },
        })
        diagnostics.startDocument()
        await Promise.resolve()
        const trace = diagnostics.generation()
        nativeSequence = 1
        trace.phase('send-admitted', 'enter')
        await Promise.resolve()
        trace.phase('awaiting-provider', 'enter')
        expect(accepted).toEqual([expect.objectContaining({
            documentSequence: 1, eventSequence: 1, phase: 'awaiting-provider',
        })])
    })

    it('refreshes stale readiness once for concurrent events and preserves their sequence', async () => {
        let nativeSequence = 1
        let handshakes = 0
        let refresh!: (value: number) => void
        const attempts: Record<string, unknown>[] = []
        const accepted: Record<string, unknown>[] = []
        const diagnostics = createGenerationDiagnostics({
            enabled: true,
            invoke: async (_command, args) => {
                const event = args.event as Record<string, unknown>
                if (event.kind === 'document-start') {
                    if (++handshakes === 1) return nativeSequence
                    return new Promise<number>(resolve => { refresh = resolve })
                }
                attempts.push(event)
                if (event.documentSequence !== nativeSequence) throw 'stale-diagnostic-document'
                accepted.push(event)
                return nativeSequence
            },
        })
        diagnostics.startDocument()
        await Promise.resolve()
        const trace = diagnostics.generation()
        nativeSequence = 2
        trace.phase('send-admitted', 'enter')
        trace.phase('awaiting-provider', 'enter')
        await vi.waitFor(() => expect(handshakes).toBe(2))
        refresh(2)
        await vi.waitFor(() => expect(accepted).toHaveLength(2))
        expect(attempts.map(event => [event.documentSequence, event.eventSequence]))
            .toEqual([[1, 1], [1, 2], [2, 1], [2, 2]])
        trace.phase('settled', 'exit')
        expect(accepted.at(-1)).toMatchObject({ documentSequence: 2, eventSequence: 3 })
        expect(handshakes).toBe(2)
    })

    it.each(['stale-diagnostic-document', 'unavailable'])('bounds retries after %s', async (failure) => {
        let handshakes = 0
        const attempts: Record<string, unknown>[] = []
        const diagnostics = createGenerationDiagnostics({
            enabled: true,
            invoke: async (_command, args) => {
                const event = args.event as Record<string, unknown>
                if (event.kind === 'document-start') return ++handshakes
                attempts.push(event)
                if (attempts.length > 2) return new Promise(() => {})
                throw failure
            },
        })
        diagnostics.startDocument()
        await Promise.resolve()
        diagnostics.generation().phase('settled', 'exit')
        await vi.waitFor(() => expect(attempts).toHaveLength(failure === 'unavailable' ? 1 : 2))
        expect(handshakes).toBe(failure === 'unavailable' ? 1 : 2)
        expect(attempts.every(event => event.eventSequence === 1)).toBe(true)
    })

    it.each(['before-refresh', 'during-refresh'])('does not adopt a later native document after retirement %s', async (stage) => {
        const lifetime = new AbortController()
        let handshakes = 0
        let fail!: (error: unknown) => void
        let refresh!: (sequence: number) => void
        const attempts: Record<string, unknown>[] = []
        const diagnostics = createGenerationDiagnostics({
            enabled: true, signal: lifetime.signal,
            invoke: (_command, args) => {
                const event = args.event as Record<string, unknown>
                if (event.kind === 'document-start') {
                    if (++handshakes === 1) return Promise.resolve(1)
                    return new Promise<number>(resolve => { refresh = resolve })
                }
                attempts.push(event)
                return new Promise((_resolve, reject) => { fail = reject })
            },
        })
        diagnostics.startDocument()
        await Promise.resolve()
        const trace = diagnostics.generation()
        trace.phase('awaiting-provider', 'enter')
        if (stage === 'before-refresh') lifetime.abort()
        fail('stale-diagnostic-document')
        if (stage === 'during-refresh') {
            await vi.waitFor(() => expect(handshakes).toBe(2))
            lifetime.abort()
            refresh(2)
        }
        await Promise.resolve()
        await Promise.resolve()
        trace.phase('settled', 'exit')
        diagnostics.lifecycle('background')
        expect(attempts).toHaveLength(1)
        expect(handshakes).toBe(stage === 'before-refresh' ? 1 : 2)
    })

    it('does not invoke native commands on the web or propagate native failures', async () => {
        let calls = 0
        const web = createGenerationDiagnostics({
            enabled: false,
            invoke: () => { calls++; throw new Error('must not be reached') },
        })
        web.startDocument()
        web.generation().phase('settled', 'exit')
        web.lifecycle('background')
        expect(calls).toBe(0)

        for (const synchronous of [false, true]) {
            let attempts = 0
            const failing = createGenerationDiagnostics({
                enabled: true,
                invoke: () => {
                    attempts++
                    if (synchronous) throw new Error('unavailable')
                    return Promise.reject(new Error('unavailable'))
                },
            })
            expect(() => failing.startDocument()).not.toThrow()
            expect(() => failing.generation().phase('settled', 'exit')).not.toThrow()
            await Promise.resolve()
            await Promise.resolve()
            failing.generation().phase('send-admitted', 'enter')
            failing.generation().phase('settled', 'exit')
            failing.lifecycle('background')
            expect(attempts).toBe(1)
        }
        let ready = false
        const laterFailure = createGenerationDiagnostics({
            enabled: true,
            invoke: async () => {
                if (!ready) { ready = true; return 1 }
                throw new Error('diagnostic command failed')
            },
        })
        laterFailure.startDocument()
        await Promise.resolve()
        expect(() => laterFailure.generation().phase('settled', 'exit')).not.toThrow()
        await Promise.resolve()
    })
})
