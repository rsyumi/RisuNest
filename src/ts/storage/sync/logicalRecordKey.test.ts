import { describe, expect, it } from 'vitest'

import {
    decodeLogicalRecordKey,
    encodeLogicalRecordKey,
    type LogicalRecordLocator,
} from './logicalRecordKey'

describe('logical record key codec', () => {
    it.each([
        [{ kind: 'root' }, 'r1:root'],
        [{ kind: 'preset', presetId: '0' }, 'r1:preset:WyIwIl0'],
        [{ kind: 'plugin', storageKey: 'plugin:key/with spaces' }, 'r1:plugin:WyJwbHVnaW46a2V5L3dpdGggc3BhY2VzIl0'],
        [{ kind: 'character', characterId: 'character-1' }, 'r1:character:WyJjaGFyYWN0ZXItMSJd'],
        [{ kind: 'conversation', characterId: 'character-1', conversationId: 'chat/1' }, 'r1:conversation:WyJjaGFyYWN0ZXItMSIsImNoYXQvMSJd'],
        [{ kind: 'asset', logicalKey: 'assets/한글 image.png' }, 'r1:asset:WyJhc3NldHMv7ZWc6riAIGltYWdlLnBuZyJd'],
        [{ kind: 'asset', logicalKey: '' }, 'r1:asset:WyIiXQ'],
        [{ kind: 'inlay', logicalKey: 'inlay\u0000id' }, 'r1:inlay:WyJpbmxheVx1MDAwMGlkIl0'],
        [{ kind: 'cold', logicalKey: 'cold-key' }, 'r1:cold:WyJjb2xkLWtleSJd'],
    ] as const)('round-trips %o using one canonical encoded form', (locator, encoded) => {
        expect(encodeLogicalRecordKey(locator as LogicalRecordLocator)).toBe(encoded)
        expect(decodeLogicalRecordKey(encoded)).toEqual(locator)
    })

    it.each([
        'r1:unknown:WyJpZCJd',
        'r1:root:WyJleHRyYSJd',
        'r1:conversation:WyJvbmx5LWNoYXJhY3RlciJd',
        'r1:character:Wzdd',
        'r1:character:WyJhIl0=',
        'r1:character:WyJcdTAwNjEiXQ',
        'r1:character:not+base64url',
    ])('rejects invalid or noncanonical input %s', (encoded) => {
        expect(() => decodeLogicalRecordKey(encoded)).toThrow(TypeError)
    })

    it.each([
        [{ kind: 'cold', logicalKey: '' }, 'r1:cold:WyIiXQ'],
        [{ kind: 'cold', logicalKey: 'cold\u0000key' }, 'r1:cold:WyJjb2xkXHUwMDAwa2V5Il0'],
    ] as const)('rejects invalid cold locator %o', (locator, encoded) => {
        expect(() => encodeLogicalRecordKey(locator)).toThrow(TypeError)
        expect(() => decodeLogicalRecordKey(encoded)).toThrow(TypeError)
    })
})
