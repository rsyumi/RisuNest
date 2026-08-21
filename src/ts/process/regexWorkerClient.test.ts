import { describe, expect, it, vi } from 'vitest'
import type { customscript } from '../storage/database.svelte'
import {
    canExecuteRegexPlanInWorker,
    getRegexExecutionPlan,
    type RegexExecutionPlan,
} from './regexExecutionPlan'
import {
    DEFAULT_REGEX_WORKER_TIMEOUT_MS,
    RegexExecutionTimeoutError,
    RegexWorkerClient,
    type RegexWorkerLike,
    type RegexWorkerRequest,
    type RegexWorkerResponse,
} from './regexWorkerClient'
import { createRegexWorkerMessageHandler } from './regexWorker'

class FakeWorker implements RegexWorkerLike {
    readonly requests: RegexWorkerRequest[] = []
    terminated = false
    private readonly messageListeners = new Set<(event: MessageEvent<RegexWorkerResponse>) => void>()
    private readonly errorListeners = new Set<(event: ErrorEvent) => void>()

    postMessage(message: RegexWorkerRequest): void {
        this.requests.push(message)
    }

    terminate(): void {
        this.terminated = true
    }

    addEventListener(type: 'message' | 'error', listener: EventListener): void {
        if (type === 'message') {
            this.messageListeners.add(listener as (event: MessageEvent<RegexWorkerResponse>) => void)
        }
        else {
            this.errorListeners.add(listener as (event: ErrorEvent) => void)
        }
    }

    removeEventListener(type: 'message' | 'error', listener: EventListener): void {
        if (type === 'message') {
            this.messageListeners.delete(listener as (event: MessageEvent<RegexWorkerResponse>) => void)
        }
        else {
            this.errorListeners.delete(listener as (event: ErrorEvent) => void)
        }
    }

    respond(response: RegexWorkerResponse): void {
        for (const listener of this.messageListeners) {
            listener({ data: response } as MessageEvent<RegexWorkerResponse>)
        }
    }

    get listenerCount(): number {
        return this.messageListeners.size + this.errorListeners.size
    }
}

class InMemoryWorker extends FakeWorker {
    private readonly handleRequest = createRegexWorkerMessageHandler((response) => {
        this.respond(response)
    })

    override postMessage(message: RegexWorkerRequest): void {
        super.postMessage(message)
        this.handleRequest(message)
    }
}

function plan(revision = 1): RegexExecutionPlan {
    return {
        revision,
        mode: 'editoutput',
        workerEligible: true,
        requiresHostExecution: false,
        entries: [{
            sourceIndex: 0,
            script: {
                comment: '',
                in: 'a',
                out: 'b',
                type: 'editoutput',
                flag: 'g',
                ableFlag: true,
            },
            pattern: 'a',
            replacement: 'b',
            flags: 'g',
            order: 0,
            actions: [],
            dynamicPattern: false,
            compiledRegex: /a/g,
            requiresHostExecution: false,
        }],
    }
}

function plannedScript(output = 'b', flag = 'g'): customscript {
    return {
        comment: '',
        in: 'a',
        out: output,
        type: 'editoutput',
        flag,
        ableFlag: true,
    }
}

describe('RegexWorkerClient', () => {
    it('registers a plan once per compact revision', async () => {
        const worker = new FakeWorker()
        const client = new RegexWorkerClient(() => worker)
        const executionPlan = plan()

        const first = client.execute(executionPlan, 'a')
        const firstExecute = worker.requests.find((message) => message.type === 'execute')
        expect(worker.requests.map((message) => message.type)).toEqual(['register', 'execute'])
        expect(firstExecute).toMatchObject({ revision: executionPlan.revision, input: 'a' })
        expect(firstExecute).not.toHaveProperty('entries')
        worker.respond({ type: 'result', id: firstExecute!.id, data: 'b', errors: [] })
        await expect(first).resolves.toEqual({ data: 'b', errors: [] })

        const second = client.execute(executionPlan, 'aa')
        const executeRequests = worker.requests.filter((message) => message.type === 'execute')
        expect(worker.requests.map((message) => message.type)).toEqual(['register', 'execute', 'execute'])
        worker.respond({ type: 'result', id: executeRequests[1].id, data: 'bb', errors: [] })
        await expect(second).resolves.toEqual({ data: 'bb', errors: [] })
    })

    it('retains only the current plan and re-registers after switching back', async () => {
        const worker = new FakeWorker()
        const client = new RegexWorkerClient(() => worker)
        const firstPlan = plan(20)
        const secondPlan = plan(21)

        for (const executionPlan of [firstPlan, secondPlan, firstPlan]) {
            const result = client.execute(executionPlan, 'a')
            const execute = worker.requests.at(-1)
            if (execute?.type !== 'execute') {
                throw new Error('Expected an execute request')
            }
            worker.respond({ type: 'result', id: execute.id, data: 'b', errors: [] })
            await result
        }

        expect(worker.requests
            .filter((message) => message.type === 'register')
            .map((message) => message.revision)).toEqual([20, 21, 20])
    })

    it('executes replacements in order and isolates invalid rules', async () => {
        const worker = new InMemoryWorker()
        const client = new RegexWorkerClient(() => worker)
        const executionPlan = plan()
        executionPlan.entries = [
            { ...executionPlan.entries[0], sourceIndex: 4, pattern: '[', replacement: 'broken' },
            { ...executionPlan.entries[0], sourceIndex: 5, pattern: 'a', replacement: 'b' },
            { ...executionPlan.entries[0], sourceIndex: 6, pattern: 'b', replacement: 'c' },
        ]

        const result = await client.execute(executionPlan, 'aa')

        expect(result.data).toBe('cc')
        expect(result.errors).toHaveLength(1)
        expect(result.errors[0].sourceIndex).toBe(4)
    })

    it('routes out-of-order responses to the matching requests', async () => {
        const worker = new FakeWorker()
        const client = new RegexWorkerClient(() => worker)
        const executionPlan = plan()

        const first = client.execute(executionPlan, 'first')
        const second = client.execute(executionPlan, 'second')
        const executes = worker.requests.filter((message) => message.type === 'execute')

        worker.respond({ type: 'result', id: executes[1].id, data: 'second-result', errors: [] })
        await expect(second).resolves.toEqual({ data: 'second-result', errors: [] })
        worker.respond({ type: 'result', id: executes[0].id, data: 'first-result', errors: [] })
        await expect(first).resolves.toEqual({ data: 'first-result', errors: [] })
    })

    it('cleans listeners and terminates an in-flight Worker on abort', async () => {
        const worker = new FakeWorker()
        const client = new RegexWorkerClient(() => worker)
        const controller = new AbortController()
        const removeListener = vi.spyOn(controller.signal, 'removeEventListener')
        const pending = client.execute(plan(), 'blocked', { signal: controller.signal })
        const outcome = pending.catch((error) => error)

        controller.abort()

        await expect(outcome).resolves.toMatchObject({ name: 'AbortError' })
        expect(removeListener).toHaveBeenCalledWith('abort', expect.any(Function))
        expect(worker.terminated).toBe(true)
        expect(worker.listenerCount).toBe(0)
    })

    it('times out at 2,000 ms, terminates without retry, and rejects stale requests', async () => {
        vi.useFakeTimers()
        try {
            const workers: FakeWorker[] = []
            const client = new RegexWorkerClient(() => {
                const worker = new FakeWorker()
                workers.push(worker)
                return worker
            })
            const timedOutPlan = plan(10)
            const timedOut = client.execute(timedOutPlan, 'blocked')
            const stale = client.execute(plan(11), 'queued')
            const timedOutOutcome = timedOut.catch((error) => error)
            const staleOutcome = stale.catch((error) => error)

            await vi.advanceTimersByTimeAsync(DEFAULT_REGEX_WORKER_TIMEOUT_MS - 1)
            expect(workers[0].terminated).toBe(false)
            await vi.advanceTimersByTimeAsync(1)

            const timeoutError = await timedOutOutcome
            expect(timeoutError).toBeInstanceOf(RegexExecutionTimeoutError)
            expect(timeoutError).toMatchObject({ category: 'regex_timeout', revision: timedOutPlan.revision })
            await expect(staleOutcome).resolves.toBe(timeoutError)
            expect(workers[0].terminated).toBe(true)
            expect(workers[0].listenerCount).toBe(0)
            expect(workers).toHaveLength(1)
            expect(workers[0].requests.filter((message) => message.type === 'execute')).toHaveLength(2)

            const replacement = client.execute(timedOutPlan, 'fresh')
            expect(workers).toHaveLength(2)
            const replacementExecute = workers[1].requests.find((message) => message.type === 'execute')!
            workers[0].respond({ type: 'result', id: replacementExecute.id, data: 'stale-result', errors: [] })
            workers[1].respond({ type: 'result', id: replacementExecute.id, data: 'fresh-result', errors: [] })
            await expect(replacement).resolves.toEqual({ data: 'fresh-result', errors: [] })
        }
        finally {
            vi.useRealTimers()
        }
    })

    it('allows only parser-independent plain replacement plans and inputs', () => {
        const ordered = getRegexExecutionPlan([plannedScript('b', 'g<order 1>')], 'editoutput')
        expect(canExecuteRegexPlanInWorker(ordered, 'plain input')).toBe(true)
        expect(canExecuteRegexPlanInWorker(
            getRegexExecutionPlan([], 'editoutput'),
            'plain input',
        )).toBe(false)
        expect(canExecuteRegexPlanInWorker(
            getRegexExecutionPlan([{ ...plannedScript(), in: '' }], 'editoutput'),
            'plain input',
        )).toBe(false)

        for (const script of [
            plannedScript('@@custom directive'),
            plannedScript('b', 'g<inject>'),
            plannedScript('b', 'g<unknown_action>'),
            plannedScript('b', 'g<cbs>'),
        ]) {
            expect(canExecuteRegexPlanInWorker(
                getRegexExecutionPlan([script], 'editoutput'),
                'plain input',
            )).toBe(false)
        }

        for (const parserOpening of ['{{value}}', '{#each}', '<user>', '<CHAR>', '<BoT>']) {
            const replacementPlan = getRegexExecutionPlan([
                plannedScript(`before ${parserOpening} after`),
            ], 'editoutput')
            expect(canExecuteRegexPlanInWorker(replacementPlan, 'plain input')).toBe(false)
            expect(canExecuteRegexPlanInWorker(ordered, `before ${parserOpening} after`)).toBe(false)
        }
    })

    it('rejects parser openings that a replacement could synthesize across a boundary', () => {
        const bracePlan = getRegexExecutionPlan([
            { ...plannedScript('{'), in: 'x' },
        ], 'editoutput')
        const anglePlan = getRegexExecutionPlan([
            { ...plannedScript('<'), in: 'x' },
        ], 'editoutput')

        expect(canExecuteRegexPlanInWorker(bracePlan, 'x{user}}')).toBe(false)
        expect(canExecuteRegexPlanInWorker(anglePlan, 'xuser>')).toBe(false)

        for (const risk of ['isolated { fragment', 'isolated < fragment']) {
            expect(canExecuteRegexPlanInWorker(
                getRegexExecutionPlan([plannedScript('safe')], 'editoutput'),
                risk,
            )).toBe(false)
        }
        for (const replacement of ['isolated { fragment', 'isolated < fragment']) {
            expect(canExecuteRegexPlanInWorker(
                getRegexExecutionPlan([plannedScript(replacement)], 'editoutput'),
                'safe input',
            )).toBe(false)
        }
    })
})
