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
    reconcilePersistentActiveCharacterIds,
} from "../storage/persistentDataRuntime.svelte";
import { restoreColdPersistentCharacter } from './coldCharacterRestore'
import { doingChat } from './generationState'
import { appendCurrentConversationMessage } from '../conversationMutations'
import type { CompleteConversationLease } from '../storage/activeWorkingSet.svelte'
import { v4 } from 'uuid'

function markGroupDirty(group: unknown) {
    markPersistentDataDirty(new TextEncoder().encode(JSON.stringify(group)).byteLength)
}

function isSelectedGroup(groupId: string): boolean {
    const selected = DBState.db.characters[get(selectedCharID)]
    return selected?.type === 'group' && selected.chaId === groupId
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
    let selectedId = get(selectedCharID)
    let group = DBState.db.characters[selectedId]
    if(group.type === 'group'){
        const res = await alertSelectChar()
        if(res){
            if(group.characters.includes(res)){
                alertError(language.errors.alreadyCharInGroup)
                return false
            }
            else{
                const loadFirstMessage = await alertConfirm(language.askLoadFirstMsg)
                const groupId = group.chaId
                const navigationGeneration = getPersistentNavigationGeneration()
                const member = await restoreColdPersistentCharacter(res, {
                    errorMessage: language.errors.coldStorageRestoreFailed,
                    isCurrent: () => (
                        getPersistentNavigationGeneration() === navigationGeneration &&
                        isSelectedGroup(groupId)
                    ),
                })
                if (!member || !isSelectedGroup(groupId)) return false
                if (get(doingChat)) return false
                selectedId = get(selectedCharID)
                group = DBState.db.characters[selectedId]
                if (group?.type !== 'group' || group.chaId !== groupId) return false
                if (group.characters.includes(res)) return false
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
                    if (!isSelectedGroup(groupId)) return false
                    selectedId = get(selectedCharID)
                    group = DBState.db.characters[selectedId]
                    if (group?.type !== 'group' || group.chaId !== groupId) return false
                    if (group.characters.includes(res) || get(doingChat)) return false
                    const selectedChat = group.chats[group.chatPage]
                    const activeSession = completeLease?.session ?? getActiveConversationSession()
                    if (
                        activeSession &&
                        !activeSession.matchesConversation(groupId, selectedChat)
                    ) return false
                    if (!hydrateCurrentGroupMemberDetail(groupId, member)) return false
                    group.characters.push(res)
                    group.characterTalks.push(1 / 6 * 4)
                    group.characterActive.push(true)
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
                            group,
                            selectedChat,
                            activeSession,
                            message,
                        )
                    }
                    markGroupDirty(group)
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
        if (activation === 'superseded') return false
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
