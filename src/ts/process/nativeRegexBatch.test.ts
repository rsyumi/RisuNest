import { describe, expect, it, vi } from 'vitest'

import type { RegexSafePlan } from './regexSafePlan'
import {
    NativeRegexBatchRejectedError,
    executeNativeRegexBatch,
    type NativeRegexBatchDependencies,
} from './nativeRegexBatch'

const safePlan: RegexSafePlan = {
    version: 1,
    entries: [{
        sourceIndex: 3,
        global: true,
        captureCount: 0,
        patternBytes: 1,
        replacementBytes: 1,
        pattern: {
            alternatives: [{ atoms: [{ kind: 'literal', value: 97 }] }],
        },
        replacement: [{ kind: 'literal', value: 'b' }],
    }],
}

function dependencies(
    invoke: NativeRegexBatchDependencies['invoke'],
): NativeRegexBatchDependencies {
    return { invoke }
}

describe('native regex batch adapter', () => {
    it('returns a complete ordered batch result from the Tauri command', async () => {
        const invoke = vi.fn(async () => ({ data: 'bbb', errors: [] }))

        await expect(executeNativeRegexBatch(safePlan, 'aaa', {}, dependencies(invoke)))
            .resolves.toEqual({ data: 'bbb', errors: [] })
        expect(invoke).toHaveBeenCalledWith('regex_execute_batch', {
            plan: safePlan,
            input: 'aaa',
        })
    })

    it('rejects the entire native result when any ordered rule error is present', async () => {
        const invoke = vi.fn(async () => ({
            data: 'partially-mutated',
            errors: [{ sourceIndex: 3, category: 'regex_shadow_compile' }],
        }))

        await expect(executeNativeRegexBatch(safePlan, 'original', {}, dependencies(invoke)))
            .rejects.toEqual(new NativeRegexBatchRejectedError([{
                sourceIndex: 3,
                category: 'regex_shadow_compile',
            }]))
    })

    it('does not invoke native execution after cancellation', async () => {
        const controller = new AbortController()
        controller.abort(new Error('cancelled before native regex'))
        const invoke = vi.fn()

        await expect(executeNativeRegexBatch(
            safePlan,
            'aaa',
            { signal: controller.signal },
            dependencies(invoke),
        )).rejects.toThrow('cancelled before native regex')
        expect(invoke).not.toHaveBeenCalled()
    })

    it('observes cancellation that races with native invocation startup', async () => {
        const controller = new AbortController()
        const invoke = vi.fn(() => {
            controller.abort(new Error('cancelled while invoking native regex'))
            return new Promise(() => {})
        })

        await expect(executeNativeRegexBatch(
            safePlan,
            'aaa',
            { signal: controller.signal },
            dependencies(invoke),
        )).rejects.toThrow('cancelled while invoking native regex')
    }, 100)

    it('does not publish a late native result after cancellation', async () => {
        const controller = new AbortController()
        let resolveInvoke!: (value: unknown) => void
        const invoke = vi.fn(() => new Promise((resolve) => {
            resolveInvoke = resolve
        }))
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
})
