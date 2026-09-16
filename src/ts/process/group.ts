import shuffle from "lodash/shuffle";
import { findCharacterbyId } from "../util";
import { alertConfirm, alertError, alertSelectChar } from "../alert";
import { language } from "src/lang";
import { get } from "svelte/store";
import { DBState, selectedCharID } from "../stores.svelte";
import {
    activateCharacter,
    acquireCompleteConversation,
    captureSelectedConversationTarget,
    flushPendingData,
    getActiveConversationSession,
    getPersistentNavigationGeneration,
    hydrateCurrentGroupMemberDetail,
    markPersistentDataDirty,
    readPersistentCharacterDetail,
    reconcilePersistentActiveCharacterIds,
} from "../storage/persistentDataRuntime.svelte";
import { doingChat } from './generationState'
import { appendCurrentConversationMessage } from '../conversationMutations'
import type { groupChat } from '../storage/database.svelte'
import { isArchivedCharacter } from '../storage/workingSetCatalog'
import type { CompleteConversationLease } from '../storage/activeWorkingSet.svelte'
import { v4 } from 'uuid'

function markGroupDirty(group: unknown) {
    markPersistentDataDirty(new TextEncoder().encode(JSON.stringify(group)).byteLength)
}

function isSelectedGroup(groupId: string): boolean {
    const selected = DBState.db.characters[get(selectedCharID)]
    return selected?.type === 'group' && selected.chaId === groupId
}

function revalidateSelectedGroup(groupId: string, pendingMemberId: string): groupChat | null {
    const selected = DBState.db.characters[get(selectedCharID)]
    if (selected?.type !== 'group' || selected.chaId !== groupId) return null
    if (selected.characters.includes(pendingMemberId)) return null
    return selected
}

async function activateSelectedGroup(groupId: string): Promise<'activated' | 'failed' | 'superseded'> {
    for (let attempt = 0; attempt < 2; attempt++) {
        if (!isSelectedGroup(groupId)) return 'superseded'
        const expectedGeneration = getPersistentNavigationGeneration() + 1
        let activated = false
        try {
            activated = await activateCharacter(groupId)
        } catch {
            activated = false
        }
        if (activated) return 'activated'
        const actualGeneration = getPersistentNavigationGeneration()
        if (
            !isSelectedGroup(groupId) ||
            (
                actualGeneration !== expectedGeneration &&
                actualGeneration !== expectedGeneration - 1
            )
        ) return 'superseded'
    }
    return 'failed'
}

async function settleGroupRollback(groupId: string): Promise<void> {
    await flushPendingData('group-membership-rollback')
    reconcilePersistentActiveCharacterIds(DBState.db, groupId)
}

export async function addGroupChar(): Promise<boolean> {
    const group = DBState.db.characters[get(selectedCharID)]
    if(group.type === 'group'){
        const res = await alertSelectChar()
        if(res){
            if(group.characters.includes(res)){
                alertError(language.errors.alreadyCharInGroup)
                return false
            }
            const candidate = DBState.db.characters.find((value) => value.chaId === res)
            if(candidate && isArchivedCharacter(candidate)){
                alertError(language.risuNest.archive.groupMemberBlocked.replace('{0}', '1'))
                return false
            }
            else{
                const loadFirstMessage = await alertConfirm(language.askLoadFirstMsg)
                const groupId = group.chaId
                if (get(doingChat)) return false
                if (!revalidateSelectedGroup(groupId, res)) return false
                const activation = await activateSelectedGroup(groupId)
                if (activation !== 'activated') return false
                if (!isSelectedGroup(groupId) || get(doingChat)) return false
                let completeLease: CompleteConversationLease | null = null
                if (loadFirstMessage) {
                    const target = captureSelectedConversationTarget()
                    if (target) {
                        completeLease = await acquireCompleteConversation(
                            'group-greeting',
                            target,
                        )
                    }
                }
                try {
                    const activatedGroup = revalidateSelectedGroup(groupId, res)
                    if (!activatedGroup || get(doingChat)) return false
                    const selectedChat = activatedGroup.chats[activatedGroup.chatPage]
                    const activeSession = completeLease?.session ?? getActiveConversationSession()
                    if (
                        activeSession &&
                        !activeSession.matchesConversation(groupId, selectedChat)
                    ) return false
                    const restoreGeneration = getPersistentNavigationGeneration()
                    const member = await readPersistentCharacterDetail(
                        res,
                        'group-member-inspection',
                    )
                    if (
                        !member ||
                        getPersistentNavigationGeneration() !== restoreGeneration ||
                        !isSelectedGroup(groupId) ||
                        get(doingChat)
                    ) return false
                    const restoredGroup = revalidateSelectedGroup(groupId, res)
                    if (!restoredGroup) return false
                    const restoredSelectedChat = restoredGroup.chats[restoredGroup.chatPage]
                    const restoredActiveSession = completeLease?.session
                        ?? getActiveConversationSession()
                    if (
                        restoredActiveSession &&
                        !restoredActiveSession.matchesConversation(groupId, restoredSelectedChat)
                    ) return false
                    if (!hydrateCurrentGroupMemberDetail(groupId, member)) return false
                    restoredGroup.characters.push(res)
                    restoredGroup.characterTalks.push(1 / 6 * 4)
                    restoredGroup.characterActive.push(true)
                    reconcilePersistentActiveCharacterIds(DBState.db, groupId)
                    if(loadFirstMessage){
                        const messageId = v4()
                        const message = {
                            role:'char',
                            data: member?.firstMessage ?? '',
                            saying: res,
                            chatId: messageId,
                        } as const
                        appendCurrentConversationMessage(
                            restoredGroup,
                            restoredSelectedChat,
                            restoredActiveSession,
                            message,
                        )
                    }
                    markGroupDirty(restoredGroup)
                    return true
                } finally {
                    completeLease?.release()
                }
            }
        }
    }
    return false
}


export async function rmCharFromGroup(index:number): Promise<boolean> {
    let selectedId = get(selectedCharID)
    let group = DBState.db.characters[selectedId]
    if(group.type === 'group'){
        if (get(doingChat)) return false
        if (index < 0 || index >= group.characters.length) return false
        const groupId = group.chaId
        const removedCharacter = group.characters[index]
        const removedTalkness = group.characterTalks[index]
        const removedActive = group.characterActive[index]
        group.characters.splice(index, 1)
        group.characterTalks.splice(index, 1)
        group.characterActive.splice(index, 1)
        markGroupDirty(group)
        const activation = await activateSelectedGroup(groupId)
        if (activation === 'activated') return true
        if (activation === 'superseded') {
            // The removal is already applied and marked dirty; false only means
            // the group is no longer selected, so residency still needs to follow
            // the mutated membership.
            reconcilePersistentActiveCharacterIds(
                DBState.db,
                DBState.db.characters[get(selectedCharID)]?.chaId ?? null,
            )
            return false
        }
        group = DBState.db.characters.find((character) => character.chaId === groupId)
        if (!group || group.type !== 'group') return false
        if (!group.characters.includes(removedCharacter)) {
            const restoredIndex = Math.min(index, group.characters.length)
            group.characters.splice(restoredIndex, 0, removedCharacter)
            group.characterTalks.splice(restoredIndex, 0, removedTalkness)
            group.characterActive.splice(restoredIndex, 0, removedActive)
        }
        markGroupDirty(group)
        await settleGroupRollback(groupId)
        return false
    }
    return false
}

export type GroupOrder = {
    id: string,
    talkness: number,
    index: number
}

export function groupOrder(chars:GroupOrder[], input:string):GroupOrder[] {
    let order:GroupOrder[] = [];
    let ids:string[] = []
    if (input) {
        const words = getWords(input)

        for (const word of words) {
            for (let char of chars) {
                const charNameChunks = getWords(findCharacterbyId(char.id).name)

                if (charNameChunks.includes(word)) {
                    order.push(char);
                    ids.push(char.id)
                    break;
                }
            }
        }
    }

    const shuffled = shuffle(chars)
    for (const char of shuffled) {
        if(ids.includes(char.id)){
            continue
        }

        const chance = char.talkness ?? 0.5

        if (chance >= Math.random()) {
            order.push(char);
            ids.push(char.id)
        }
    }

    while (order.length === 0) {
        order.push(chars[Math.floor(Math.random() * chars.length)]);
    }

    return order;
}

function getWords(data:string){
    const matches =  data.split(/\n| /g)
    let words:string[] = []
    if(!matches){
        return [data]
    }
    for(const match of matches){
        words.push(match.toLocaleLowerCase())
    }
    return words
}
