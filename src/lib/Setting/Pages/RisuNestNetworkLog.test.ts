import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, unmount, tick } from 'svelte'
import { language } from 'src/lang'
import { NativeExportError } from 'src/ts/storage/nativeExportFile'
const mock = vi.hoisted(() => ({ call: vi.fn(), download: vi.fn() }))
vi.mock('src/ts/network/networkLog', async original => ({ ...await original<typeof import('src/ts/network/networkLog')>(), networkLog: mock.call, downloadNetworkLog: mock.download }))
import Component from './RisuNestNetworkLog.svelte'
const settings = { enabled: false, retain: false, hideAuth: true, recentCount: 200, recentMib: 1024, retainedMib: 1024, bufferMib: 32, captureBinary: false }
const body = { observedBytes: 5, capturedBytes: 5, binary: false, truncated: false, representation: 'observed' }
const record = { id: 'synthetic', source: 'native-http', startedAt: 0, elapsedMs: 42, retained: false, headerMasking: true, state: 'complete', captureError: null,
    request: { method: 'POST', url: 'https://synthetic.example/chat', status: null, headers: [{ name: 'authorization', value: 'Bearer sk••••••abcdef' }], body },
    response: { method: null, url: 'https://synthetic.example/chat', status: 200, headers: [], body }, streamKind: null, eventBytes: 0 }
const t = language.risuNest.networkLog
let host: HTMLElement, component: ReturnType<typeof mount>
async function settle() { for (let i = 0; i < 18; i++) { await Promise.resolve(); await tick() } }
function refreshButton() { return host.querySelector<HTMLButtonElement>(`button[aria-label="${t.refresh}"]`)! }
function button(text: string) { const result = [...host.querySelectorAll('button')].find(b => b.textContent?.trim() === text); if (!result) throw new Error(`missing ${text}`); return result }
beforeEach(async () => {
    vi.clearAllMocks()
    mock.call.mockImplementation(async request => {
        switch (request.op) {
            case 'settings': return settings
            case 'configure': return request.settings
            case 'list': return { items: [{ ...record, ...record.request, id: 'synthetic', status: 200, capturedBytes: 10 }], hasMore: false, recentBytes: 10, retainedBytes: 0, warning: null }
            case 'detail': return record
            case 'body': return { text: 'hello', offset: 0, nextOffset: 5, total: 5, binary: false }
            case 'copy': return 'hello'
        }
    })
    host = document.createElement('div'); document.body.append(host); component = mount(Component, { target: host }); await settle()
})
afterEach(async () => { await unmount(component); host.remove() })
describe('native network settings', () => {
    it('groups interleaved retries by their operation while leaving old records separate', async () => {
        const original = mock.call.getMockImplementation()!
        mock.call.mockImplementation(async request => request.op === 'list' ? {
            items: ['retry-2', 'legacy', 'retry-1'].map(id => ({
                ...record, ...record.request, id, status: 503, capturedBytes: 0,
                ...(id === 'legacy' ? {} : { operationId: 'synthetic-operation' }),
            })), hasMore: false, recentBytes: 0, retainedBytes: 0, warning: null,
        } : original(request))
        refreshButton().click()
        await settle()
        const groups = [...host.querySelectorAll('.request-group')]
        expect(groups).toHaveLength(2)
        expect(groups[0].querySelectorAll('.request-row')).toHaveLength(2)
        expect(groups[0].textContent).toContain('synthetic-operation')
        expect(groups[1].querySelectorAll('.request-row')).toHaveLength(1)
        groups[0].querySelectorAll<HTMLButtonElement>('.request-row')[1].click()
        await settle()
        expect(mock.call).toHaveBeenCalledWith({ op: 'detail', id: 'retry-1' })
    })

    it('shows opt-in defaults and records once enabled', async () => {
        const inputs = [...host.querySelectorAll<HTMLInputElement>('input[type=checkbox]')]
        expect(inputs.map(input => input.checked)).toEqual([false, false, true, false])
        inputs[0].checked = true; inputs[0].dispatchEvent(new Event('change', { bubbles: true })); await settle()
        expect(mock.call).toHaveBeenCalledWith({ op: 'configure', settings: { ...settings, enabled: true } })
    })
    it('configures the shared buffer with the approved presets', async () => {
        const select = host.querySelector<HTMLSelectElement>('#network-bufferMib')!
        expect(select.value).toBe('32')
        expect([...select.options].map(option => Number(option.value))).toEqual([4, 8, 16, 32, 64, 128])
        for (const value of [4, 8, 16, 32, 64, 128]) {
            select.value = String(value); select.dispatchEvent(new Event('change', { bubbles: true })); await settle()
            expect(mock.call).toHaveBeenCalledWith({ op: 'configure', settings: { ...settings, bufferMib: value } })
            expect(select.value).toBe(String(value))
        }
    })
    it('toggles binary capture independently of the recording buffer', async () => {
        const input = [...host.querySelectorAll('label')].find(label => label.textContent?.trim() === t.captureBinary)!.querySelector('input')!
        expect(input.checked).toBe(false)
        for (const captureBinary of [true, false]) {
            input.checked = captureBinary; input.dispatchEvent(new Event('change', { bubbles: true })); await settle()
            expect(mock.call).toHaveBeenCalledWith({ op: 'configure', settings: { ...settings, captureBinary } })
            expect(input.checked).toBe(captureBinary)
            expect(host.querySelector<HTMLSelectElement>('#network-bufferMib')!.value).toBe('32')
        }
    })
    it('opens headers and bodies, copies, retains, and downloads one request', async () => {
        host.querySelector<HTMLButtonElement>('.request-row')!.click(); await settle()
        expect(host.textContent).toContain('Bearer sk••••••abcdef')
        button(t.responseBody).click(); await settle(); expect(host.querySelector('pre')?.textContent).toBe('hello')
        const writeText = vi.fn().mockResolvedValue(undefined); Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })
        button(t.copy).click(); await settle(); expect(writeText).toHaveBeenCalledWith('hello')
        button(t.keep).click(); await settle(); expect(mock.call).toHaveBeenCalledWith({ op: 'retain', id: 'synthetic' })
        button(t.download).click(); await settle(); expect(mock.download).toHaveBeenCalledWith('synthetic')
        mock.download.mockRejectedValueOnce(Object.assign(new NativeExportError('network-log-export-failed'), { warningCodes: ['partial-destination-may-remain'] }))
        button(t.download).click(); await settle()
        expect(host.textContent).toContain(`${t.exportFailed} ${language.screenshotPartialDestinationMayRemain}`)
    })
    it('shows SSE sequence and arrival times across pages without marking a known boundary partial', async () => {
        const original = mock.call.getMockImplementation()!
        const stream = 'data: value\n\n'.repeat(51)
        mock.call.mockImplementation(async request => {
            if (request.op === 'detail') return { ...record, streamKind: 'sse' }
            if (request.op === 'body') return { text: stream.slice(request.offset), offset: request.offset, nextOffset: stream.length, total: stream.length, binary: false }
            if (request.op === 'arrivalTimes') return { times: request.ends.map((end: number) => end * 10), next: request.from + 100 }
            return original(request)
        })
        host.querySelector<HTMLButtonElement>('.request-row')!.click(); await settle(); button(t.events).click(); await settle()
        let events = [...host.querySelectorAll('.event-row > summary')]
        expect(events).toHaveLength(50)
        expect(events[0].textContent).toContain('#1')
        expect(events[49].textContent).toContain('#50')
        expect(events[0].querySelector('time')?.getAttribute('title')).toBe(new Date(130).toISOString())
        const arrivalCalls = () => mock.call.mock.calls.map(([request]) => request).filter(request => request.op === 'arrivalTimes')
        expect(arrivalCalls().at(-1).from).toBe(0)
        button(t.next).click(); await settle()
        events = [...host.querySelectorAll('.event-row > summary')]
        expect(events).toHaveLength(1)
        expect(events[0].textContent).toContain('#51')
        expect(events[0].textContent).not.toContain(t.fragment)
        expect(arrivalCalls().at(-1).from).toBe(100)
        button(t.previous).click(); await settle()
        expect(host.querySelector('.event-row > summary')?.textContent).toContain('#1')
        expect(arrivalCalls().at(-1).from).toBe(0)
    })
    it('shows an omitted binary body as its own reason instead of a partial capture', async () => {
        const original = mock.call.getMockImplementation()!
        const skipped = { ...record, captureError: 'binary-skipped', response: { ...record.response, body: { ...body, capturedBytes: 0, binary: true, truncated: true } } }
        mock.call.mockImplementation(async request => {
            if (request.op === 'list') return { items: [{ ...skipped, ...skipped.request, status: 200, capturedBytes: 5, incomplete: false }], hasMore: false, recentBytes: 5, retainedBytes: 0, warning: null }
            if (request.op === 'detail') return skipped
            if (request.op === 'body') return { text: '', offset: 0, nextOffset: 0, total: 0, binary: true }
            return original(request)
        })
        refreshButton().click(); await settle()
        expect(host.querySelector('.request-row')?.textContent).not.toContain(t.partial)
        host.querySelector<HTMLButtonElement>('.request-row')!.click(); await settle()
        expect(host.textContent).toContain(t.binarySkipped)
        button(t.responseBody).click(); await settle()
        expect(host.textContent).not.toContain(t.bodyLimited)
        expect(host.querySelector('pre')?.textContent).toBe(t.noBody)
    })
    it('keeps polling the list but rereads only a request that is still active', async () => {
        await unmount(component); host.remove()
        vi.useFakeTimers()
        try {
            host = document.createElement('div'); document.body.append(host); component = mount(Component, { target: host }); await settle()
            host.querySelector<HTMLButtonElement>('.request-row')!.click(); await settle()
            const count = (op: string) => mock.call.mock.calls.filter(([request]) => request.op === op).length
            const lists = count('list'), details = count('detail')
            await vi.advanceTimersByTimeAsync(3100); await settle()
            expect(count('list')).toBeGreaterThan(lists)
            expect(count('detail')).toBe(details)
            const original = mock.call.getMockImplementation()!
            mock.call.mockImplementation(async request => request.op === 'detail' ? { ...record, state: 'active' } : original(request))
            refreshButton().click(); await settle()
            const active = count('detail')
            await vi.advanceTimersByTimeAsync(3100); await settle()
            expect(count('detail')).toBeGreaterThan(active)
        } finally { vi.useRealTimers() }
    })
    it('renders the selected WebSocket message after asynchronous loading', async () => {
        const original = mock.call.getMockImplementation()!
        mock.call.mockImplementation(async request => request.op === 'detail' ? { ...record, streamKind: 'websocket' } : request.op === 'events'
            ? { text: JSON.stringify({ elapsedMs: 10, side: 'response', offset: 0, length: 5, kind: 'text' }) + '\n', offset: 0, nextOffset: 90, total: 90, binary: false } : original(request))
        host.querySelector<HTMLButtonElement>('.request-row')!.click(); await settle(); button(t.messages).click(); await settle()
        const message = [...host.querySelectorAll('button')].find(b => b.textContent?.includes('10 ms'))!; message.click(); await settle()
        expect(host.querySelector('pre')?.textContent).toBe('hello')
    })
})
