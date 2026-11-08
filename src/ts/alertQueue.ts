import { writable, type Readable } from 'svelte/store'
import type { alertData } from './alert'

const STATUS_TYPES: ReadonlySet<alertData['type']> = new Set(['wait', 'progress', 'toast'])
/** Notices without an answer, where an identical copy in a row adds nothing. */
const REPEATABLE_TYPES: ReadonlySet<alertData['type']> = new Set(['normal', 'error', 'markdown', 'wait2'])
/** The next queued dialog waits this long, so a double click or a repeated key cannot answer it unseen. */
const NEXT_DIALOG_GAP_MS = 150

/** Whether a published alert value is a dialog or the gap before the next queued one, rather than a status. */
export function showsDialog(value: alertData): boolean {
    return !!value.dialogPending || (value.type !== 'none' && !STATUS_TYPES.has(value.type))
}

interface Dialog {
    data: alertData
    closed: Promise<string>
    close(msg: string): void
}

export interface AlertQueue extends Readable<alertData> {
    /** `none` closes the visible dialog; status types replace the status; other types queue a dialog. */
    set(value: alertData): void
    update(change: (value: alertData) => alertData): void
    /** Queues a dialog and resolves with the `msg` it closes with. */
    open(value: alertData): Promise<string>
    clearStatus(): void
    /** Resolves once no dialog is visible or queued. */
    idle(): Promise<void>
    dialogVisible(): boolean
    hasDialogs(): boolean
}

function sameNotice(a: alertData, b: alertData): boolean {
    return a.type === b.type && a.msg === b.msg && (a.submsg ?? '') === (b.submsg ?? '') && !!a.wrapCode === !!b.wrapCode
}

/**
 * Alerts share one screen slot. Loading, progress and toast form a status that the next status
 * replaces; every other alert is a dialog that waits its turn and answers only its own caller.
 */
export function createAlertQueue(initial: alertData, options: { gapMs?: number } = {}): AlertQueue {
    const gapMs = options.gapMs ?? NEXT_DIALOG_GAP_MS
    const dialogs: Dialog[] = []
    let status: alertData | null = null
    let headVisible = false
    let gapTimer: ReturnType<typeof setTimeout> | null = null
    let idleWaiters: (() => void)[] = []
    let current = initial
    const store = writable(initial)

    function show(value: alertData) {
        if (value === current || (value.type === 'none' && current.type === 'none')) return
        current = value
        store.set(value)
    }
    function publish(closedMsg = '') {
        if (headVisible) show(dialogs[0].data)
        else if (gapTimer) show({ type: 'none', msg: closedMsg, dialogPending: true })
        else show(status ?? { type: 'none', msg: closedMsg })
    }
    function showHead() {
        gapTimer = null
        headVisible = dialogs.length > 0
        publish()
    }
    function enqueue(value: alertData): Promise<string> {
        // A dialog supersedes the status set before it, as when a result replaces its loading overlay.
        status = null
        const last = dialogs.at(-1)
        if (last && REPEATABLE_TYPES.has(value.type) && sameNotice(last.data, value)) {
            publish()
            return last.closed
        }
        let close!: (msg: string) => void
        const closed = new Promise<string>((resolve) => { close = resolve })
        dialogs.push({ data: value, closed, close })
        if (!headVisible && !gapTimer) headVisible = true
        publish()
        return closed
    }
    function closeVisible(msg: string) {
        const dialog = dialogs.shift()!
        headVisible = false
        if (dialogs.length > 0) {
            gapTimer = setTimeout(showHead, gapMs)
        } else {
            const waiters = idleWaiters
            idleWaiters = []
            for (const resolve of waiters) resolve()
        }
        publish(msg)
        dialog.close(msg)
    }
    function set(value: alertData) {
        if (value.type === 'none') {
            if (headVisible) closeVisible(value.msg)
            else {
                status = null
                publish(value.msg)
            }
        } else if (STATUS_TYPES.has(value.type)) {
            status = value
            publish()
        } else {
            void enqueue(value)
        }
    }

    return {
        subscribe: store.subscribe,
        set,
        update: (change) => set(change(current)),
        open: enqueue,
        clearStatus() {
            status = null
            publish()
        },
        idle: () => dialogs.length === 0
            ? Promise.resolve()
            : new Promise<void>((resolve) => { idleWaiters.push(resolve) }),
        dialogVisible: () => headVisible,
        hasDialogs: () => dialogs.length > 0,
    }
}

export const alertStore = createAlertQueue({ type: 'none', msg: 'n' })

export function alertMd(msg: string, wrapCode = false) {
    alertStore.set({ type: 'markdown', msg, ...(wrapCode ? { wrapCode: true } : {}) })
}
