import { Packr } from 'msgpackr/index-no-eval'

const legacyEncoder = new Packr({ useRecords: false })
const legacyHeader = new Uint8Array([0, 82, 73, 83, 85, 83, 65, 86, 69, 0, 7])

export function encodeLegacyConversationProjection(value: unknown): Uint8Array {
    const payload = legacyEncoder.encode(value)
    const result = new Uint8Array(legacyHeader.length + payload.length)
    result.set(legacyHeader)
    result.set(payload, legacyHeader.length)
    return result
}
