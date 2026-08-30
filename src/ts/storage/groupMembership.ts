export const DEFAULT_GROUP_TALKNESS = 1 / 6 * 4

export interface GroupMemberReferences {
    characters: string[]
    characterTalks?: number[]
    characterActive?: boolean[]
}

export interface RetainedGroupMemberReferences {
    characters: string[]
    characterTalks: number[]
    characterActive: boolean[]
}

/**
 * Rebuilds the parallel group-membership arrays (characters, characterTalks,
 * characterActive) with the given member ids removed, keeping the remaining
 * entries aligned by their original indices.
 */
export function removeGroupMemberReferences(
    group: GroupMemberReferences,
    removedIds: ReadonlySet<string>,
): RetainedGroupMemberReferences {
    const retained = group.characters
        .map((id, index) => ({ id, index }))
        .filter(({ id }) => !removedIds.has(id))
    return {
        characters: retained.map(({ id }) => id),
        characterTalks: retained.map(
            ({ index }) => group.characterTalks?.[index] ?? DEFAULT_GROUP_TALKNESS,
        ),
        characterActive: retained.map(
            ({ index }) => group.characterActive?.[index] ?? true,
        ),
    }
}
