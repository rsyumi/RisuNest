import { failureRecord } from '../failureRecord.mjs'

const RECORD_TIMEOUT_MS = 5_000

/**
 * The text a CDP driver throws for a failed `Runtime.evaluate`: `text`, followed by the `cause`
 * chain of the exception when it is an Error the page still holds. `call(method, params)` sends one
 * CDP command. `text` alone comes back when the exception is not an Error, has no cause, or its
 * record cannot be read.
 */
export async function evaluationErrorText(call, exceptionDetails, text) {
    const exception = exceptionDetails?.exception
    const objectId = exception?.subtype === 'error' ? exception.objectId : undefined
    if (!objectId) return text
    let timer
    try {
        const response = await Promise.race([
            Promise.resolve().then(() => call('Runtime.callFunctionOn', {
                objectId,
                functionDeclaration: `function () { return (${failureRecord})(this) }`,
                returnByValue: true,
            })),
            new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('timed out')), RECORD_TIMEOUT_MS) }),
        ])
        const record = response?.exceptionDetails ? undefined : response?.result?.value
        if (!Array.isArray(record?.causes)) return text
        return `${text}\ncauses${record.causesCut ? ` (cut: ${record.causesCut})` : ''}: ${JSON.stringify(record.causes)}`
    } catch {
        return text
    } finally {
        clearTimeout(timer)
        Promise.resolve().then(() => call('Runtime.releaseObject', { objectId })).catch(() => {})
    }
}
