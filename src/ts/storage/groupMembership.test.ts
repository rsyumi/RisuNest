import { describe, expect, it } from 'vitest'

import { DEFAULT_GROUP_TALKNESS, removeGroupMemberReferences } from './groupMembership'

describe('group membership', () => {
    it('removes members while keeping the parallel arrays aligned by original index', () => {
        const retained = removeGroupMemberReferences({
            characters: ['a', 'b', 'c'],
            characterTalks: [0.1, 0.2, 0.3],
            characterActive: [true, false, true],
        }, new Set(['b']))

        expect(retained).toEqual({
            characters: ['a', 'c'],
            characterTalks: [0.1, 0.3],
            characterActive: [true, true],
        })
    })

    it('fills missing talkness and activity entries with their defaults', () => {
        const retained = removeGroupMemberReferences({
            characters: ['a', 'b'],
        }, new Set(['missing']))

        expect(retained).toEqual({
            characters: ['a', 'b'],
            characterTalks: [DEFAULT_GROUP_TALKNESS, DEFAULT_GROUP_TALKNESS],
            characterActive: [true, true],
        })
    })
})
