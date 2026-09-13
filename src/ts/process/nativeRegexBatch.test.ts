import { describe, expect, it, vi } from 'vitest'

import type { RegexSafePlan } from './regexSafePlan'
import {
    NativeRegexBatchRejectedError,
    executeNativeRegexBatch,
    isNativeRegexTauriRuntime,
    tryExecuteNativeRegexBatch,
    type NativeRegexBatchDependencies,
    type NativeRegexBatchRouteDependencies,
} from './nativeRegexBatch'
import { getRegexExecutionPlan } from './regexExecutionPlan'
import { makeRegexFixture } from './tests/phase1Fixtures'

const safePlan: RegexSafePlan = {
    version: 1,
    entries: [
        {
            sourceIndex: 3,
            global: true,
            captureCount: 0,
            patternBytes: 1,
            replacementBytes: 1,
            pattern: {
                alternatives: [{ atoms: [{ kind: 'literal', value: 97 }] }],
            },
            replacement: [{ kind: 'literal', value: 'b' }],
        },
    ],
}

function dependencies(
    invoke: NativeRegexBatchDependencies['invoke'],
): NativeRegexBatchDependencies {
    return {
        invoke,
        createRequestId: () => '11111111-1111-4111-8111-111111111111',
    }
}

describe('native regex batch adapter', () => {
    it.each([
        ['Windows Tauri', 'windows', {}, true],
        ['Android Tauri', 'android', {}, true],
        ['Linux Tauri', 'linux', {}, true],
        ['iOS Tauri', 'ios', {}, true],
        ['macOS Tauri', 'macos', {}, false],
        ['browser Web', 'linux', undefined, false],
        ['unknown Tauri', 'unknown', {}, false],
    ] as const)(
        'detects the supported %s runtime',
        (_name, os, tauriInternals, expected) => {
            expect(isNativeRegexTauriRuntime(os, tauriInternals)).toBe(expected)
        },
    )

    it('returns a complete ordered batch result from the Tauri command', async () => {
        const invoke = vi.fn(async () => ({ data: 'bbb', errors: [] }))

        await expect(
            executeNativeRegexBatch(safePlan, 'aaa', {}, dependencies(invoke)),
        ).resolves.toEqual({ data: 'bbb', errors: [] })
        expect(invoke).toHaveBeenCalledWith('regex_execute_batch', {
            requestId: '11111111-1111-4111-8111-111111111111',
            plan: safePlan,
            input: 'aaa',
        })
    })

    it('rejects the entire native result when any ordered rule error is present', async () => {
        const invoke = vi.fn(async () => ({
            data: 'partially-mutated',
            errors: [{ sourceIndex: 3, category: 'regex_shadow_compile' }],
        }))

        await expect(
            executeNativeRegexBatch(safePlan, 'original', {}, dependencies(invoke)),
        ).rejects.toEqual(
            new NativeRegexBatchRejectedError([
                {
                    sourceIndex: 3,
                    category: 'regex_shadow_compile',
                },
            ]),
        )
    })

    it('does not invoke native execution after cancellation', async () => {
        const controller = new AbortController()
        controller.abort(new Error('cancelled before native regex'))
        const invoke = vi.fn()

        await expect(
            executeNativeRegexBatch(
                safePlan,
                'aaa',
                { signal: controller.signal },
                dependencies(invoke),
            ),
        ).rejects.toThrow('cancelled before native regex')
        expect(invoke).not.toHaveBeenCalled()
    })

    it('observes cancellation that races with native invocation startup', async () => {
        const controller = new AbortController()
        const invoke = vi.fn(() => {
            controller.abort(new Error('cancelled while invoking native regex'))
            return new Promise(() => {})
        })

        await expect(
            executeNativeRegexBatch(
                safePlan,
                'aaa',
                { signal: controller.signal },
                dependencies(invoke),
            ),
        ).rejects.toThrow('cancelled while invoking native regex')
    }, 100)

    it('does not publish a late native result after cancellation', async () => {
        const controller = new AbortController()
        let resolveInvoke!: (value: unknown) => void
        const invoke = vi.fn(
            () =>
                new Promise((resolve) => {
                    resolveInvoke = resolve
                }),
        )
        const pending = executeNativeRegexBatch(
            safePlan,
            'aaa',
            { signal: controller.signal },
            dependencies(invoke),
        )

        controller.abort(new Error('cancelled during native regex'))
        resolveInvoke({ data: 'bbb', errors: [] })

        await expect(pending).rejects.toThrow('cancelled during native regex')
    })

    it('dispatches cooperative native cancellation for the active request', async () => {
        const controller = new AbortController()
        const requestId = '11111111-1111-4111-8111-111111111111'
        const invoke = vi.fn((command: string) =>
            command === 'regex_execute_batch' ? new Promise(() => {}) : Promise.resolve(undefined),
        )
        const pending = executeNativeRegexBatch(
            safePlan,
            'aaa',
            { signal: controller.signal },
            {
                invoke,
                createRequestId: () => requestId,
            },
        )

        controller.abort(new Error('cancelled native execution'))

        await expect(pending).rejects.toThrow('cancelled native execution')
        expect(invoke).toHaveBeenNthCalledWith(1, 'regex_execute_batch', {
            requestId,
            plan: safePlan,
            input: 'aaa',
        })
        expect(invoke).toHaveBeenNthCalledWith(2, 'regex_cancel_batch', {
            requestId,
        })
    })

    it('routes only the bounded 500-rule and 256 KiB supported Tauri boundary', async () => {
        const fixture = makeRegexFixture(500, 256 * 1024)
        const input = fixture.input.slice(0, 256 * 1024)
        const plan = getRegexExecutionPlan(fixture.scripts, 'editoutput')
        const invoke = vi.fn(async () => ({ data: 'native-output', errors: [] }))
        const routeDependencies: NativeRegexBatchRouteDependencies = {
            isSupportedTauri: () => true,
            invoke,
        }

        await expect(
            tryExecuteNativeRegexBatch(plan, input, {}, routeDependencies),
        ).resolves.toEqual({ data: 'native-output', errors: [] })
        expect(invoke).toHaveBeenCalledOnce()
    })

    it.each([
        ['20 rules', 20, 256 * 1024, true],
        ['100 rules', 100, 256 * 1024, true],
        ['500 rules and 32 KiB', 500, 32 * 1024, true],
        ['Web', 500, 256 * 1024, false],
    ] as const)(
        'keeps measured losing or unsupported %s batches on the Worker',
        async (_name, ruleCount, inputBytes, isSupportedTauri) => {
            const fixture = makeRegexFixture(ruleCount, inputBytes)
            const plan = getRegexExecutionPlan(fixture.scripts, 'editoutput')
            const invoke = vi.fn()

            await expect(
                tryExecuteNativeRegexBatch(
                    plan,
                    fixture.input.slice(0, inputBytes),
                    {},
                    { isSupportedTauri: () => isSupportedTauri, invoke },
                ),
            ).resolves.toBeUndefined()
            expect(invoke).not.toHaveBeenCalled()
        },
    )

    it('keeps a mixed 500-rule plan on the Worker when any rule is not Rust-safe', async () => {
        const fixture = makeRegexFixture(500, 256 * 1024)
        fixture.scripts[499].in = '(?=rule-499)rule-499'
        const plan = getRegexExecutionPlan(fixture.scripts, 'editoutput')
        const invoke = vi.fn()

        await expect(
            tryExecuteNativeRegexBatch(
                plan,
                fixture.input.slice(0, 256 * 1024),
                {},
                { isSupportedTauri: () => true, invoke },
            ),
        ).resolves.toBeUndefined()
        expect(invoke).not.toHaveBeenCalled()
    })
})
