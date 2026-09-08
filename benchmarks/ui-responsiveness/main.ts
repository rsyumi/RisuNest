import { mount } from 'svelte'
import '../../src/styles.css'
import Harness from './Harness.svelte'
import {
    FIXTURE_CHARACTER_COUNT,
    FIXTURE_MESSAGE_COUNT,
    fixtureCanonicalText,
} from './fixture'

type LongTaskRecord = { startTime: number; duration: number }

declare global {
    interface Window {
        __risuUiBenchmarkClock: {
            startedAt: number
            frameTimes: number[]
            longTasks: LongTaskRecord[]
        }
        __risuUiBenchmarkReady?: boolean
        __risuUiRunNavigation?: () => Promise<unknown>
    }
}

const clock = window.__risuUiBenchmarkClock
const firstChatStartedAt = performance.now()
let firstChatReadyAt = 0
let firstChatVerification: unknown = null
const fixtureHashPromise = crypto.subtle
    .digest('SHA-256', new TextEncoder().encode(fixtureCanonicalText()))
    .then((digest) =>
        Array.from(new Uint8Array(digest), (byte) =>
            byte.toString(16).padStart(2, '0'),
        ).join(''),
    )

const instance = mount(Harness, {
    target: document.getElementById('app')!,
    props: {
        onFirstChatReady: (verification) => {
            firstChatVerification = verification
            firstChatReadyAt = performance.now()
            window.__risuUiBenchmarkReady = true
        },
    },
})

function summarizeDurations(values: readonly number[]) {
    return {
        count: values.length,
        totalMs: values.reduce((sum, value) => sum + value, 0),
        maxMs: values.length === 0 ? 0 : Math.max(...values),
    }
}

window.__risuUiRunNavigation = async () => {
    if (!firstChatReadyAt) throw new Error('First synthetic chat is not ready')
    const navigationStartedAt = performance.now()
    const result = await instance.navigateToNextCharacter()
    const fixtureHash = await fixtureHashPromise
    const readyAt = performance.now()
    const frameGaps = clock.frameTimes
        .slice(1)
        .map((time, index) => time - clock.frameTimes[index])
        .filter((duration) => duration > 20)
    const longTasks = clock.longTasks
        .filter(
            (entry) =>
                entry.startTime >= clock.startedAt &&
                entry.startTime <= readyAt,
        )
        .map((entry) => entry.duration)
    return {
        fixture: {
            schemaVersion: 1,
            characterCount: FIXTURE_CHARACTER_COUNT,
            messageCountPerCharacter: FIXTURE_MESSAGE_COUNT,
            contentHash: fixtureHash,
        },
        validOutput: result.validOutput,
        verification: {
            firstChat: firstChatVerification,
            navigation: result,
        },
        totalReadyDurationMs: readyAt - clock.startedAt,
        moduleLoadToFirstChatReadyDurationMs:
            firstChatReadyAt - clock.startedAt,
        firstChatDurationMs: firstChatReadyAt - firstChatStartedAt,
        navigationDurationMs: readyAt - navigationStartedAt,
        longTasks: summarizeDurations(longTasks),
        animationFrameGaps: summarizeDurations(frameGaps),
    }
}
