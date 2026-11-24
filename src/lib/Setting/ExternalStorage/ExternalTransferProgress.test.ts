// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, settled as domSettled, unmount } from 'svelte'
import { SvelteMap } from 'svelte/reactivity'
const native = vi.hoisted(() => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke, Channel: class { onmessage = (_value: unknown) => {} } }))
import ExternalTransferProgress from './ExternalTransferProgress.svelte'
import { externalStorageStrings } from './strings'
import { beginExternalProgress, clearExternalProgress } from 'src/ts/storage/sync/external/progress'
import type { ExternalJobSummary } from 'src/ts/storage/sync/external/types'
import { formatRisuNestStorageBytes as bytes } from 'src/ts/storage/risuNestStorageDashboard'
const strings = externalStorageStrings('ko')
let component: ReturnType<typeof mount> | undefined
let target: HTMLDivElement
const show = async (props: Record<string, unknown> = {}) => {
    target = document.createElement('div'); document.body.append(target)
    component = mount(ExternalTransferProgress, { target, props: { connectionId: 'synthetic', strings, ...props } })
    await domSettled()
}
const counterRows = () => [...target.querySelectorAll('dt')].map(term => [term.textContent, term.nextElementSibling?.textContent])
const objectsValue = (size: number, count: string) => `${bytes(size)} · ${strings.transfer.objects.replace('{0}', count)}`
afterEach(async () => {
    if (component) await unmount(component)
    component = undefined; target?.remove(); clearExternalProgress('synthetic'); vi.useRealTimers()
})
describe('external storage shared panel', () => {
    it('shows the staging phase share beside its operation after completed transfers without inventing a total or completing the operation', async () => {
        await show({ onboarding: true })
        const run = beginExternalProgress('synthetic', 'binding')
        let report!: (reading: unknown) => void
        native.invoke.mockImplementation(async (_name, { progress }) => { report = progress.onmessage })
        await run.invoke('synthetic', {})
        const statusValues = () => [...target.querySelectorAll('[role="status"] .tabular-nums')].map(node => node.textContent)
        const reading = { sequence: 1, stage: 'applying', preparedBytes: '0', uploadedBytes: '0', downloadedBytes: '1024', uploadedObjects: '0', downloadedObjects: '100' }
        report({ ...reading, staging: { phase: 'validate', done: 25, total: 100 } }); await domSettled()
        expect(target.textContent).toContain(strings.staging.validate)
        expect(statusValues()).toEqual([strings.transfer.connecting, '25%'])
        expect(counterRows()).toEqual([[strings.transfer.downloaded, objectsValue(1024, '100')]])
        expect(target.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('25')
        report({ ...reading, sequence: 2, staging: { phase: 'project', done: 3, total: null } }); await domSettled()
        expect(target.textContent).toContain(strings.staging.project)
        expect(statusValues()).toEqual([strings.transfer.connecting])
        expect(target.querySelector('[role="progressbar"]')?.hasAttribute('aria-valuenow')).toBe(false)
        report({ ...reading, sequence: 3, staging: { phase: 'check', done: 100, total: 100 } }); await domSettled()
        expect(target.textContent).toContain(strings.staging.check)
        expect(target.textContent).not.toContain(strings.transfer.connectionComplete)
        run.stage('finalizing'); await domSettled()
        expect(target.textContent).not.toContain(strings.staging.check)
        expect(target.querySelector('[role="progressbar"]')?.hasAttribute('aria-valuenow')).toBe(false)
        run.finish('complete'); await domSettled()
        expect(target.textContent).toContain(strings.transfer.connectionComplete)
    })
    it.each(['backup', 'restore'] as const)('shows %s network speed from live job samples and hides it when paused', async kind => {
        vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'performance'] })
        const network = { id: kind, atMs: 0, sentBytes: '0', receivedBytes: '0', sending: kind === 'backup', receiving: kind === 'restore' }
        const transfer = { sequence: 1, stage: 'uploading' as const, preparedBytes: '0', uploadedBytes: '0', downloadedBytes: '0', uploadedObjects: '0', downloadedObjects: '0', network }
        const job: ExternalJobSummary = { id: kind, connectionId: 'synthetic', kind, state: 'running', phase: kind === 'backup' ? 'preparing' : 'downloading', completedBytes: '0', completedItems: '0', startedAtMs: '0', updatedAtMs: '0', transfer }
        const jobs = new SvelteMap([['current', job]])
        target = document.createElement('div'); document.body.append(target)
        component = mount(ExternalTransferProgress, { target, props: { connectionId: 'synthetic', strings, get job() { return jobs.get('current') } } })
        await domSettled()
        jobs.set('current', { ...job, transfer: { ...transfer, sequence: 2, network: { ...network, atMs: 1000, sentBytes: '1048576', receivedBytes: '2097152' } } })
        await domSettled()
        expect(target.textContent).toContain(kind === 'backup' ? `${strings.transfer.uploadSpeed} 1.0 MiB/s` : `${strings.transfer.downloadSpeed} 2.0 MiB/s`)
        jobs.set('current', { ...jobs.get('current')!, phase: 'applying-local', kind: 'restore' }); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
        jobs.set('current', { ...job, state: 'waiting', phase: 'paused' }); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
    })
    it('shows partial network transfer before any item completes and hides speed during local work and completion', async () => {
        vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'performance'] })
        await show({ onboarding: true })
        const run = beginExternalProgress('synthetic', 'binding')
        let report!: (reading: unknown) => void
        native.invoke.mockImplementation(async (_name, { progress }) => { report = progress.onmessage })
        await run.invoke('synthetic', {})
        const reading = { sequence: 1, stage: 'downloading', preparedBytes: '0', uploadedBytes: '0', downloadedBytes: '0', uploadedObjects: '0', downloadedObjects: '0', network: { id: 'native', atMs: 0, sentBytes: '0', receivedBytes: '0', sending: false, receiving: true } }
        report(reading); await domSettled()
        report({ ...reading, sequence: 2, network: { ...reading.network, atMs: 1000, receivedBytes: '1048576' } }); await domSettled()
        expect(target.textContent).toContain(`${strings.transfer.downloadSpeed} 1.0 MiB/s`)
        expect(target.textContent).not.toContain(strings.transfer.downloaded)
        run.stage('waiting'); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
        run.stage('applying'); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
        run.finish('complete'); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
    })
    it('shows the speed on the heading row, outside its live region, until the transfer finishes', async () => {
        vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'performance'] })
        await show()
        const run = beginExternalProgress('synthetic', 'binding')
        let report!: (reading: unknown) => void
        native.invoke.mockImplementation(async (_name, { progress }) => { report = progress.onmessage })
        await run.invoke('synthetic', {})
        const reading = { sequence: 1, stage: 'downloading', preparedBytes: '0', uploadedBytes: '0', downloadedBytes: '0', uploadedObjects: '0', downloadedObjects: '0', network: { id: 'native', atMs: 0, sentBytes: '0', receivedBytes: '0', sending: false, receiving: true } }
        report(reading); await domSettled()
        const status = target.querySelector('[data-setting-progress] [role="status"]')!
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
        report({ ...reading, sequence: 2, network: { ...reading.network, atMs: 1000, receivedBytes: '1048576' } }); await domSettled()
        const speed = target.querySelector('[data-transfer-speed]')!
        expect(speed.textContent).toContain(`${strings.transfer.downloadSpeed} 1.0 MiB/s`)
        expect(speed.parentElement).toBe(status.parentElement)
        expect(speed.closest('[role="status"]')).toBeNull()
        expect(status.parentElement!.nextElementSibling?.getAttribute('role')).toBe('progressbar')
        run.stage('applying'); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
        run.finish('complete'); await domSettled()
        expect(target.querySelector('[data-transfer-speed]')).toBeNull()
    })
    it('observes an automatic operation started after mounting and keeps details collapsed', async () => {
        vi.useFakeTimers(); await show()
        const run = beginExternalProgress('synthetic', 'sync')
        native.invoke.mockImplementation(async (_name, { progress }) => progress.onmessage({ sequence: 1, stage: 'uploading', uploadedBytes: '4096', uploadedObjects: '2', downloadedBytes: '1024', downloadedObjects: '1', preparedBytes: '2000' }))
        await run.invoke('synthetic', {})
        await vi.advanceTimersByTimeAsync(650); await domSettled()
        expect(target.textContent).toContain(strings.transfer.uploading)
        expect(target.querySelector('details')?.open).toBe(false)
        expect(counterRows()).toEqual([
            [strings.transfer.prepared, bytes(2000)],
            [strings.transfer.uploaded, objectsValue(4096, '2')],
            [strings.transfer.downloaded, objectsValue(1024, '1')],
        ])
        expect(target.querySelector('[role="progressbar"]')?.hasAttribute('aria-valuenow')).toBe(false)
        run.finish('complete'); await domSettled()
        expect(target.textContent).toContain(strings.transfer.syncComplete)
    })
    it('keeps a finished sync turn in progress while receiving continues, including during the next hidden check', async () => {
        const flags = new SvelteMap([['catchingUp', true]])
        target = document.createElement('div'); document.body.append(target)
        component = mount(ExternalTransferProgress, { target, props: { connectionId: 'synthetic', strings, get catchingUp() { return flags.get('catchingUp') } } })
        const status = () => target.querySelector('[data-external-progress="sync"]')?.textContent ?? ''
        const first = beginExternalProgress('synthetic', 'sync')
        first.stage('applying'); first.finish('complete'); await domSettled()
        expect(status()).toContain(strings.transfer.syncing)
        expect(status()).not.toContain(strings.transfer.syncComplete)
        expect(target.querySelector('[role="progressbar"]')?.hasAttribute('aria-valuenow')).toBe(false)
        const next = beginExternalProgress('synthetic', 'sync'); await domSettled()
        expect(status()).toContain(strings.transfer.syncing)
        expect(status()).not.toContain(strings.transfer.syncComplete)
        next.stage('applying'); next.finish('complete'); flags.set('catchingUp', false); await domSettled()
        expect(status()).toContain(strings.transfer.syncComplete)
    })
    it('shows onboarding details directly and remains active while applying a completed download', async () => {
        const run = beginExternalProgress('synthetic', 'binding')
        run.stage('applying')
        await show({ onboarding: true, syncing: true })
        expect(target.textContent).toContain(strings.transfer.applying)
        expect(target.querySelector('details')).toBeNull()
        expect(target.querySelector('[role="progressbar"]')?.hasAttribute('aria-valuenow')).toBe(false)
        run.finish('failed'); await domSettled()
        expect(target.textContent).toContain(strings.failed)
        expect(target.textContent).not.toContain(strings.transfer.connectionComplete)
    })
    it('names sync data and file downloads separately without repeating the title', async () => {
        const binding = beginExternalProgress('synthetic', 'binding')
        binding.stage('downloading')
        const download = beginExternalProgress('synthetic', 'download')
        download.items(1, 3)
        await show()
        expect(target.querySelector('[data-external-progress="binding"]')?.textContent).toContain(strings.transfer.downloading)
        expect(target.querySelector('[data-external-progress="binding"]')?.textContent).not.toContain(strings.transfer.filesDownloading)
        expect(target.querySelector('[data-external-progress="download"]')?.textContent?.split(strings.transfer.filesDownloading)).toHaveLength(2)
        binding.finish('complete'); download.finish('complete')
    })
    it('keeps each disclosure independent when transfers overlap', async () => {
        native.invoke.mockImplementation(async (_name, { progress }) => progress.onmessage({ sequence: 1, stage: 'uploading', uploadedBytes: '4', uploadedObjects: '1', downloadedBytes: '0', downloadedObjects: '0', preparedBytes: '0' }))
        const binding = beginExternalProgress('synthetic', 'binding')
        await binding.invoke('synthetic', {})
        const download = beginExternalProgress('synthetic', 'download')
        download.items(1, 3)
        await show()
        const panels = target.querySelectorAll('details')
        expect(panels).toHaveLength(2)
        panels[0].open = true
        panels[0].dispatchEvent(new Event('toggle'))
        await domSettled()
        expect(panels[0].open).toBe(true)
        expect(panels[1].open).toBe(false)
        binding.finish('complete'); download.finish('complete')
    })
    it('stops activity while a connection waits for confirmation and resumes afterwards', async () => {
        const binding = beginExternalProgress('synthetic', 'binding')
        binding.stage('waiting')
        await show({ onboarding: true, syncing: true })
        expect(target.textContent).toContain(strings.transfer.waiting)
        expect(target.querySelector('[class*="animate-spin"], [class*="animate-pulse"]')).toBeNull()
        binding.stage('applying'); await domSettled()
        expect(target.textContent).toContain(strings.transfer.applying)
        expect(target.querySelector('[class*="animate-spin"]')).not.toBeNull()
        binding.finish('complete')
    })
    it('displays preparation and confirmed upload separately, then shows only the local restore preparation share', async () => {
        const base: ExternalJobSummary = { id: 'backup', connectionId: 'synthetic', kind: 'backup', state: 'running', phase: 'prepare', counters: 'prepared', completedBytes: '20', totalBytes: '100', completedItems: '1', totalItems: '5', startedAtMs: '0', updatedAtMs: '0', transfer: { sequence: 1, stage: 'uploading', preparedBytes: '0', downloadedBytes: '0', downloadedObjects: '0', uploadedBytes: '50', uploadedObjects: '2' } }
        await show({ job: base, onboarding: true })
        expect(target.textContent).toContain(strings.jobCounters.prepared)
        expect(counterRows()).toEqual([[strings.transfer.uploaded, objectsValue(50, '2')]])
        expect(target.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('20')
        await unmount(component!); target.remove(); component = undefined
        const preparing: ExternalJobSummary = { ...base, kind: 'restore', phase: 'preparing-local', counters: 'transferred', completedBytes: '100', transfer: { ...base.transfer!, staging: { phase: 'project', done: 30, total: 120 } } }
        await show({ job: preparing, onboarding: true })
        expect(target.textContent).toContain(strings.staging.project)
        expect([...target.querySelectorAll('[role="status"] .tabular-nums')].map(node => node.textContent)).toEqual([strings.restorePhases['preparing-local'], '25%'])
        expect(target.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('25')
        await unmount(component!); target.remove(); component = undefined
        // A share left over from preparation never carries into applying the restore.
        await show({ job: { ...preparing, phase: 'applying-local', transfer: { ...preparing.transfer!, staging: { phase: 'project', done: 120, total: 120 } } }, onboarding: true })
        expect(target.textContent).toContain(strings.restorePhases['applying-local'])
        expect(target.textContent).not.toContain(strings.staging.project)
        expect(target.querySelector('[role="progressbar"]')?.hasAttribute('aria-valuenow')).toBe(false)
    })
})
