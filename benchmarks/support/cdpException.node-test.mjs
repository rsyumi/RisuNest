import assert from 'node:assert/strict'
import { Session } from 'node:inspector/promises'
import test from 'node:test'

import { evaluationErrorText } from './cdpException.mjs'

// The V8 inspector of this process answers the same Runtime commands a WebView's does.
async function inspect(work) {
    const session = new Session()
    session.connect()
    try {
        return await work((method, params) => session.post(method, params))
    } finally {
        session.disconnect()
    }
}

async function failedEvaluation(call, expression) {
    const response = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true, userGesture: true })
    assert.ok(response.exceptionDetails, 'the expression fails')
    const text = response.exceptionDetails.exception?.description ?? response.exceptionDetails.text
    return { text, message: await evaluationErrorText(call, response.exceptionDetails, text) }
}

test('a rejected evaluation names each cause of the error after its description', async () => {
    await inspect(async (call) => {
        const { text, message } = await failedEvaluation(call, `(async () => {
            const root = new RangeError('synthetic quota')
            throw new Error('Lua Worker could not start', { cause: new TypeError('Worker script was blocked', { cause: root }) })
        })()`)
        assert.ok(text.startsWith('Error: Lua Worker could not start'))
        assert.ok(message.startsWith(`${text}\ncauses: `))
        const causes = JSON.parse(message.slice(`${text}\ncauses: `.length))
        assert.deepEqual(causes.map(({ name, message }) => ({ name, message })), [
            { name: 'TypeError', message: 'Worker script was blocked' },
            { name: 'RangeError', message: 'synthetic quota' },
        ])
        assert.ok(causes.every(cause => cause.stack.startsWith(`${cause.name}: ${cause.message}`)))
    })
})

test('a thrown chain longer than the limit or with a repeated cause says where it was cut', async () => {
    await inspect(async (call) => {
        const long = await failedEvaluation(call, `(() => {
            let error = new Error('level 0')
            for (let level = 1; level <= 10; level++) error = new Error('level ' + level, { cause: error })
            throw error
        })()`)
        const causes = JSON.parse(long.message.slice(`${long.text}\ncauses (cut: limit): `.length))
        assert.deepEqual(causes.map(cause => cause.message), ['level 9', 'level 8', 'level 7', 'level 6', 'level 5', 'level 4', 'level 3', 'level 2'])
        const cycle = await failedEvaluation(call, `(() => {
            const first = new Error('first')
            first.cause = new Error('second', { cause: first })
            throw first
        })()`)
        assert.ok(cycle.message.startsWith(`${cycle.text}\ncauses (cut: cycle): `))
        assert.deepEqual(JSON.parse(cycle.message.slice(`${cycle.text}\ncauses (cut: cycle): `.length)).map(cause => cause.message), ['second'])
    })
})

test('an error without a cause, a thrown value that is not an Error, or an unreadable record keeps the description', async () => {
    await inspect(async (call) => {
        for (const expression of [
            `Promise.reject(new Error('alone'))`,
            `Promise.reject('plain string')`,
            `(() => { throw { message: 'objectish', cause: new Error('hidden') } })()`,
        ]) {
            const { text, message } = await failedEvaluation(call, expression)
            assert.equal(message, text, expression)
        }
        const response = await call('Runtime.evaluate', {
            expression: `Promise.reject(new Error('outer', { cause: new Error('inner') }))`, awaitPromise: true, returnByValue: true,
        })
        const refused = async (method, params) => {
            if (method === 'Runtime.callFunctionOn') throw new Error('Target closed')
            return call(method, params)
        }
        assert.equal(await evaluationErrorText(refused, response.exceptionDetails, 'Error: outer'), 'Error: outer')
    })
})
