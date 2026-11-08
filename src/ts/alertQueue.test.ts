import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import { createAlertQueue, showsDialog } from './alertQueue'

const none = { type: 'none', msg: '' } as const

function queue(options: { gapMs?: number } = { gapMs: 0 }) {
    const alerts = createAlertQueue({ ...none }, options)
    const published: string[] = []
    alerts.subscribe((value) => { published.push(`${value.type}:${value.msg}`) })
    published.length = 0
    return { alerts, published }
}

async function settled<T>(promise: Promise<T>) {
    let state: { value: T } | undefined
    void promise.then((value) => { state = { value } })
    await Promise.resolve()
    await Promise.resolve()
    return state
}

describe('alert queue', () => {
    beforeEach(() => { vi.useFakeTimers() })
    afterEach(() => { vi.useRealTimers() })

    it('shows dialogs in arrival order and answers each caller with its own result', async () => {
        const { alerts } = queue()
        const first = alerts.open({ type: 'ask', msg: 'first' })
        const second = alerts.open({ type: 'tos', msg: 'tos' })
        expect(get(alerts)).toMatchObject({ type: 'ask', msg: 'first' })

        alerts.set({ type: 'none', msg: 'yes' })
        await expect(first).resolves.toBe('yes')
        expect(get(alerts)).toEqual({ type: 'none', msg: 'yes', dialogPending: true })
        await vi.advanceTimersByTimeAsync(0)
        expect(get(alerts)).toMatchObject({ type: 'tos', msg: 'tos' })

        alerts.set({ type: 'none', msg: 'no' })
        await expect(second).resolves.toBe('no')
        expect(get(alerts)).toEqual({ type: 'none', msg: 'no' })
    })

    it('keeps a dialog open when a toast or loading status arrives', async () => {
        const { alerts } = queue()
        const terms = alerts.open({ type: 'tos', msg: 'tos' })
        alerts.set({ type: 'toast', msg: 'Copied' })
        alerts.set({ type: 'wait', msg: 'Loading' })
        expect(get(alerts)).toMatchObject({ type: 'tos' })
        expect(await settled(terms)).toBeUndefined()

        alerts.set({ type: 'none', msg: 'yes' })
        await expect(terms).resolves.toBe('yes')
        expect(get(alerts)).toEqual({ type: 'wait', msg: 'Loading' })
    })

    it('does not republish a visible dialog for a status written behind it', () => {
        const { alerts, published } = queue()
        alerts.set({ type: 'normal', msg: 'Saved' })
        alerts.set({ type: 'progress', msg: '10%' })
        alerts.set({ type: 'progress', msg: '20%' })
        expect(published).toEqual(['normal:Saved'])
    })

    it('replaces a status with the next status and clears it on none', () => {
        const { alerts } = queue()
        alerts.set({ type: 'wait', msg: 'Loading' })
        alerts.set({ type: 'progress', msg: '50%' })
        expect(get(alerts)).toEqual({ type: 'progress', msg: '50%' })
        alerts.set({ type: 'none', msg: '' })
        expect(get(alerts)).toEqual(none)
    })

    it('lets a dialog supersede the status set before it but not one set after it', async () => {
        const { alerts } = queue()
        alerts.set({ type: 'wait', msg: 'Loading' })
        const result = alerts.open({ type: 'normal', msg: 'Done' })
        alerts.set({ type: 'none', msg: '' })
        await result
        expect(get(alerts)).toEqual({ type: 'none', msg: '' })

        const next = alerts.open({ type: 'normal', msg: 'Again' })
        alerts.set({ type: 'wait', msg: 'Later' })
        alerts.set({ type: 'none', msg: '' })
        await next
        expect(get(alerts)).toEqual({ type: 'wait', msg: 'Later' })
    })

    it('clears only the status through clearStatus', async () => {
        const { alerts } = queue()
        const question = alerts.open({ type: 'ask', msg: 'Continue?' })
        alerts.set({ type: 'wait', msg: 'Loading' })
        alerts.clearStatus()
        expect(get(alerts)).toMatchObject({ type: 'ask' })
        expect(await settled(question)).toBeUndefined()

        alerts.set({ type: 'none', msg: 'no' })
        await question
        expect(get(alerts)).toEqual({ type: 'none', msg: 'no' })
    })

    it('waits for the gap, marked as pending, before showing a dialog queued behind a closed one', async () => {
        const { alerts, published } = queue({})
        const first = alerts.open({ type: 'ask', msg: 'first' })
        const second = alerts.open({ type: 'ask', msg: 'second' })
        alerts.set({ type: 'wait', msg: 'Loading' })
        alerts.set({ type: 'none', msg: 'yes' })
        await expect(first).resolves.toBe('yes')
        expect(get(alerts)).toEqual({ type: 'none', msg: 'yes', dialogPending: true })
        expect(showsDialog(get(alerts))).toBe(true)
        expect(alerts.dialogVisible()).toBe(false)
        expect(alerts.hasDialogs()).toBe(true)

        alerts.set({ type: 'none', msg: 'yes' })
        alerts.set({ type: 'toast', msg: 'Copied' })
        await vi.advanceTimersByTimeAsync(149)
        expect(get(alerts)).toEqual({ type: 'none', msg: 'yes', dialogPending: true })
        expect(await settled(second)).toBeUndefined()

        await vi.advanceTimersByTimeAsync(1)
        expect(get(alerts)).toMatchObject({ type: 'ask', msg: 'second' })
        alerts.set({ type: 'none', msg: 'no' })
        await expect(second).resolves.toBe('no')
        expect(get(alerts)).toEqual({ type: 'toast', msg: 'Copied' })
        expect(showsDialog(get(alerts))).toBe(false)
        expect(published).toEqual(['ask:first', 'none:yes', 'ask:second', 'toast:Copied'])
    })

    it('shows a dialog opened after the queue emptied without a gap', async () => {
        const { alerts } = queue({})
        const first = alerts.open({ type: 'ask', msg: 'first' })
        alerts.set({ type: 'none', msg: 'yes' })
        await first
        void alerts.open({ type: 'select', msg: 'a||b' })
        expect(get(alerts)).toMatchObject({ type: 'select', msg: 'a||b' })
    })

    it('merges an identical warning into the last queued one', async () => {
        const { alerts } = queue()
        const first = alerts.open({ type: 'error', msg: 'Network failed', submsg: 'Check proxy' })
        const repeat = alerts.open({ type: 'error', msg: 'Network failed', submsg: 'Check proxy', stackTrace: 'other' })
        expect(repeat).toBe(first)
        alerts.set({ type: 'none', msg: '' })
        await first
        expect(alerts.hasDialogs()).toBe(false)
        expect(get(alerts)).toEqual(none)
    })

    it('keeps warnings that differ or that only match an earlier entry', async () => {
        const { alerts } = queue()
        void alerts.open({ type: 'error', msg: 'A' })
        void alerts.open({ type: 'error', msg: 'A', submsg: 'detail' })
        void alerts.open({ type: 'normal', msg: 'A' })
        void alerts.open({ type: 'error', msg: 'A' })
        const seen: string[] = []
        while (alerts.hasDialogs()) {
            await vi.advanceTimersByTimeAsync(0)
            const value = get(alerts)
            seen.push(`${value.type}:${value.msg}:${value.submsg ?? ''}`)
            alerts.set({ type: 'none', msg: '' })
        }
        expect(seen).toEqual(['error:A:', 'error:A:detail', 'normal:A:', 'error:A:'])
    })

    it('shows the same warning again after the user closed it', async () => {
        const { alerts } = queue()
        const first = alerts.open({ type: 'normal', msg: 'Saved' })
        alerts.set({ type: 'none', msg: '' })
        await first
        const second = alerts.open({ type: 'normal', msg: 'Saved' })
        expect(second).not.toBe(first)
        expect(get(alerts)).toMatchObject({ type: 'normal', msg: 'Saved' })
    })

    it('never merges questions', async () => {
        const { alerts } = queue()
        const first = alerts.open({ type: 'ask', msg: 'Delete?' })
        const second = alerts.open({ type: 'ask', msg: 'Delete?' })
        alerts.set({ type: 'none', msg: 'yes' })
        await vi.advanceTimersByTimeAsync(0)
        alerts.set({ type: 'none', msg: 'no' })
        await expect(first).resolves.toBe('yes')
        await expect(second).resolves.toBe('no')
    })

    it('clears the status when a merged warning arrives', async () => {
        const { alerts } = queue()
        const first = alerts.open({ type: 'error', msg: 'Failed' })
        alerts.set({ type: 'wait', msg: 'Retrying' })
        void alerts.open({ type: 'error', msg: 'Failed' })
        alerts.set({ type: 'none', msg: '' })
        await first
        expect(get(alerts)).toEqual(none)
    })

    it('queues dialogs written with set and with update', async () => {
        const { alerts } = queue()
        alerts.set({ type: 'normal', msg: 'one' })
        alerts.update(() => ({ type: 'normal', msg: 'two' }))
        expect(get(alerts)).toMatchObject({ msg: 'one' })
        alerts.update(() => ({ type: 'none', msg: '' }))
        await vi.advanceTimersByTimeAsync(0)
        expect(get(alerts)).toMatchObject({ msg: 'two' })
    })

    it('resolves idle once every dialog has closed', async () => {
        const { alerts } = queue()
        expect(await settled(alerts.idle())).toEqual({ value: undefined })
        alerts.set({ type: 'wait', msg: 'Loading' })
        expect(await settled(alerts.idle())).toEqual({ value: undefined })

        void alerts.open({ type: 'normal', msg: 'one' })
        void alerts.open({ type: 'normal', msg: 'two' })
        const idle = alerts.idle()
        alerts.set({ type: 'none', msg: '' })
        expect(await settled(idle)).toBeUndefined()
        await vi.advanceTimersByTimeAsync(0)
        alerts.set({ type: 'none', msg: '' })
        expect(await settled(idle)).toEqual({ value: undefined })
    })
})
