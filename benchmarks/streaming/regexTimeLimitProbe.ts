import { mount, settled as domSettled, unmount } from 'svelte'
import RegexFixture from './RegexFixture.svelte'
import { DBState } from '../../src/ts/stores.svelte'
import type { customscript } from '../../src/ts/storage/database.svelte'
import { processScriptFull, resetScriptCache } from '../../src/ts/process/scripts'

const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))

export type RegexProbeWorkload = 'mixed' | 'cbs'

function rules(count: number, workload: RegexProbeWorkload): customscript[] {
    return Array.from({ length: count }, (_, index) => ({
        type: 'editdisplay', comment: 'Synthetic regex measurement', ableFlag: true, flag: 'g',
        in: index % 2 ? `absent-${index}-` : `token-${index}-` + (workload === 'cbs' ? '(\\w*)' : ''),
        out: workload === 'mixed' ? (index % 10 === 0 ? '{{char}}' : `changed-${index}-`) :
            (index % 4 === 0 ? '{{#if {{greater_equal::{{chat_index}}::{{? {{lastmessageid}}-3}}}}}}'
                : '{{#if {{greater_equal::{{chat_index}}::0}}}}') +
            '<span data-cbs-index="{{chat_index}}">' +
            '{{#if {{and::{{? {{chat_index}}={{lastmessageid}}}}::{{getvar::enabled}}}}}}' +
            '{{button::+::syntheticAction}}{{/if}}' +
            Array.from({ length: 24 }, (_, variable) => `{{getvar::probe${variable}}}`).join(' ') +
            ' capture:$1</span>{{/if}}',
    }))
}

function source(bytes: number, count: number, marker: string, workload: RegexProbeWorkload) {
    const tokens = Array.from({ length: count }, (_, index) => `token-${index}-`).join(' ')
    const text = workload === 'mixed'
        ? (tokens + ' synthetic text ').repeat(Math.ceil(bytes / Math.max(tokens.length, 1))).slice(0, bytes)
        : (tokens + ' synthetic text '.repeat(Math.ceil(bytes / 16))).slice(0, bytes)
    return `<p>${text}</p><p>${marker}</p>`
}

async function rendered(markers: string[], expectVariables = false, deadline = 60_000) {
    const start = performance.now()
    while (performance.now() - start < deadline) {
        const rows = [...document.querySelectorAll('[data-regex-row]')]
        if (rows.length === markers.length && rows.every((row, i) => row.textContent?.includes(markers[i])
            && !/token-\d*[02468]-|\{\{/.test(row.textContent))
            && (!expectVariables || rows.at(-1)!.textContent?.includes('value23'))) {
            await frame()
            return
        }
        await frame()
    }
    throw new Error('Synthetic regex rows did not finish rendering')
}

export async function runRegexTimeLimitProbe(cleanup: () => Promise<void>, counts = [0, 50, 200, 500], sizes = [2048, 30720], repeats = 3, rowCount = 30, workload: RegexProbeWorkload = 'mixed') {
    await cleanup()
    DBState.db.streamingThoughtMode = 'off'
    DBState.db.streamingDeferDisplayProcessing = false
    const measurements: object[] = []
    for (const bytes of sizes) for (const count of counts) for (let sample = 0; sample < repeats; sample++) {
        resetScriptCache()
        const nonce = `${bytes}-${count}-${sample}`
        const markers = Array.from({ length: rowCount }, (_, index) => `ROW-${nonce}-${index}-READY`)
        const initial = markers.map((marker) => source(bytes, count, marker, workload))
        const chat = DBState.db.characters[0].chats[0]
        chat.message = initial.map((data) => ({ role: 'char', data }))
        chat.scriptstate = { $enabled: '1', ...Object.fromEntries(Array.from({ length: 24 }, (_, i) => [`$probe${i}`, `value${i}`])) }
        const scripts = rules(count, workload)
        const started = performance.now()
        const fixture = mount(RegexFixture, { target: document.getElementById('app')!, props: { scripts, initial } })
        await domSettled()
        await rendered(markers, workload === 'cbs' && count > 0)
        const openMs = performance.now() - started
        const updates: number[] = []
        for (let update = 0; update < 5; update++) {
            markers[rowCount - 1] = `UPDATE-${nonce}-${update}-READY`
            const started = performance.now()
            fixture.publish(rowCount - 1, source(bytes, count, markers[rowCount - 1], workload))
            await domSettled()
            await rendered(markers, workload === 'cbs' && count > 0)
            updates.push(performance.now() - started)
        }
        measurements.push({ workload, rules: count, rows: rowCount, bytes, sample, openMs, updateMs: updates })
        await unmount(fixture)
    }
    return { passed: true, cases: [{ id: 'regex-time-limit', measurements }] }
}

export async function verifyRegexTimeoutResponsiveness() {
    const character = { ...DBState.db.characters[0], customscript: [{
        comment: 'Synthetic unclosed status blocks', type: 'editdisplay', in: '\\[(.*?)\\|(.*?)\\|(.*?)\\]', out: 'bad', flag: 'gs', ableFlag: true,
    }] }
    const text = '[x|y|'.repeat(1600)
    let timerMs = -1
    const start = performance.now()
    const timer = setTimeout(() => { timerMs = performance.now() - start }, 50)
    try {
        const rows = await Promise.all(Array.from({ length: 30 }, (_, i) => processScriptFull(character, text + i, 'editdisplay', i, {}, { cache: 'bypass' })))
        const firstMs = performance.now() - start
        const repeated = performance.now()
        const again = await processScriptFull(character, text + 0, 'editdisplay', 0, {}, { cache: 'bypass' })
        return { firstMs, repeatMs: performance.now() - repeated, timerMs,
            unchanged: rows.every((row, i) => row.data === text + i) && again.data === text + 0 }
    } finally { clearTimeout(timer) }
}
