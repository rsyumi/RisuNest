import { get } from 'svelte/store'
import { DBState, selectedCharID } from '../stores.svelte'
import { parseKeyValue } from '../util'
import { getCurrentChat } from '../storage/database.svelte'
import type { Chat, Database } from '../storage/database.svelte'

export function getChatVarFromConversation(
    database: Database,
    characterId: string,
    chat: Chat,
    key: string,
): string {
    const char = database.characters.find((candidate) => candidate.chaId === characterId)
    if (!char) return 'null'
    // Display reads must not dirty a fresh or metadata-only conversation.
    const state = chat.scriptstate?.['$' + key]
    if (state === undefined || state === null) {
        const defaultVariables = parseKeyValue(char.defaultVariables).concat(
            parseKeyValue(database.templateDefaultVariables),
        )
        return defaultVariables.find(([name]) => name === key)?.[1] ?? 'null'
    }
    return state.toString()
}

export function setChatVarOnConversation(
    chat: Chat,
    key: string,
    value: string,
): boolean {
    chat.scriptstate ??= {}
    const stateKey = '$' + key
    if (chat.scriptstate[stateKey] === value) return false
    chat.scriptstate[stateKey] = value
    return true
}

export function getChatVar(key:string): string {
    const selectedChar = get(selectedCharID)
    const char = DBState.db.characters[selectedChar]
    if(!char){
        return 'null'
    }
    const chat = char.chats[char.chatPage]
    return getChatVarFromConversation(DBState.db, char.chaId, chat, key)
}

export function setChatVar(key:string, value:string): boolean {
    const selectedChar = get(selectedCharID)
    const chat = DBState.db.characters[selectedChar].chats[DBState.db.characters[selectedChar].chatPage]
    return setChatVarOnConversation(chat, key, value)
}

export function getGLChatVar(key:string): string {
    console.log('getGLChatVar', key)
    const chat = getCurrentChat()
    return chat?.GLGlobalVariables?.[key]
}

export function setGLChatVar(key:string, value:string) {
    console.log('setGLChatVar', key, value)
    const chat = getCurrentChat()
    if(chat){
        console.log('setGLChatVar', key, value, chat.GLGlobalVariables)
        chat.GLGlobalVariables ??= {}
        chat.GLGlobalVariables[key] = value
    }
}

export function getGlobalChatVar(key:string): string {
    const vt = getGLChatVar(key)
    if(vt !== 'null' && vt){
        return vt
    }
    return DBState.db.globalChatVariables[key] ?? 'null'
}

export function setGlobalChatVar(key:string, value:string) {
    if(getCurrentChat()?.useLocallySetGlobalVariables){
        setGLChatVar(key, value)
        return
    }
    else if(getGLChatVar(key) !== undefined){
        delete getCurrentChat().GLGlobalVariables[key]
    }
    DBState.db.globalChatVariables[key] = value
}

export function isLocallyHandledGlobalChatVar(key:string): boolean {
    return !!getGLChatVar(key)
}

export function removeLocallyHandledGlobalChatVar(key:string): boolean {
    if(getGLChatVar(key) !== undefined){
        delete getCurrentChat().GLGlobalVariables[key]
        return true
    }
    return false
}
