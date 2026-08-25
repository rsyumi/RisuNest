import { v4 } from 'uuid';
import { alertError, alertInput, alertNormal, alertStore, alertWait } from '../alert';
import { get, writable } from 'svelte/store';
import {
    setDatabase,
    type character,
    saveImage,
    type Chat,
    getCurrentChat,
    setCurrentChat,
    getDatabase,
    type groupChat,
} from '../storage/database.svelte';
import { selectedCharID } from '../stores.svelte';
import { sleep } from '../util';
import type { DataConnection, Peer } from 'peerjs';
import { readImage } from '../globalApi.svelte';
import { doingChat } from '../process/index.svelte';
import {
    activateCharacter,
    invalidatePersistentNavigation,
    upsertPersistentCompleteCharacter,
} from '../storage/persistentDataRuntime.svelte';

async function importPeerJS(){
    return await import('peerjs');
}

interface ReciveFirst{
    type: 'receive-char',
    data: character
}
interface RequestFirst{
    type: 'request-char'
}
interface ReciveAsset{
    type: 'receive-asset',
    id: string,
    data: Uint8Array
}
interface RequestSync{
    type: 'request-chat-sync',
    id: string,
    data: Chat
}
interface ReciveSync{
    type: 'receive-chat',
    data: Chat
}
interface RequestChatSafe{
    type: 'request-chat-safe',
    id: string
}
interface ResponseChatSafe{
    type: 'response-chat-safe'
    data: boolean,
    id: string
}
interface RequestChat{
    type: 'request-chat'
}

type ReciveData = ReciveFirst|RequestFirst|ReciveAsset|RequestSync|ReciveSync|RequestChatSafe|ResponseChatSafe|RequestChat

type CompleteCharacter = character | groupChat

const MULTIUSER_TEMP_CHARACTER_ID = '§temp'

export function normalizeIncomingCharacter(incoming:character):character{
    const detached = safeStructuredClone(incoming)
    detached.chaId = MULTIUSER_TEMP_CHARACTER_ID
    detached.chatPage = 0
    detached.chats = (detached.chats ?? []).filter((chat) => !!chat)
    const assignedIds = new Set<string>()
    for (const chat of detached.chats) {
        let id = v4()
        while (!id || assignedIds.has(id)) id = v4()
        chat.id = id
        assignedIds.add(id)
    }
    return detached
}

export function normalizeIncomingChat(chat:Chat, fallbackId?:string):Chat{
    if(!chat.id){
        chat.id = fallbackId || v4()
    }
    return chat
}

export function normalizeIncomingChatForCurrent(incoming: Chat, current?: Chat): Chat {
    const normalized = safeStructuredClone(incoming)
    normalized.id = current?.id || v4()
    return normalized
}

export interface MultiuserReceiveControllerDependencies {
    upsertCompleteCharacter(
        characterId: string,
        reason: string,
        createOrMutate: (
            existing: CompleteCharacter | null,
        ) => CompleteCharacter | Promise<CompleteCharacter>,
        options?: { includeInCharacterOrder?: boolean },
    ): Promise<boolean>
    activateCharacter(characterId: string): Promise<boolean>
    invalidateNavigation(): void
    deselect(): void
    onChatCommitted(chat: Chat): void
}

export interface MultiuserReceiveController {
    receiveCharacter(character: character): Promise<void>
    receiveChat(chat: Chat): Promise<void>
    close(): void
}

export function createMultiuserReceiveController(
    dependencies: MultiuserReceiveControllerDependencies,
): MultiuserReceiveController {
    let closed = false
    let ready = false
    let operationTail = Promise.resolve()

    const enqueue = (operation: () => Promise<void>): Promise<void> => {
        const result = operationTail.then(async () => {
            if (closed) return
            await operation()
        })
        operationTail = result.then(
            () => undefined,
            () => undefined,
        )
        return result
    }

    return {
        receiveCharacter(incoming) {
            const detached = normalizeIncomingCharacter(incoming)
            return enqueue(async () => {
                ready = false
                const committed = await dependencies.upsertCompleteCharacter(
                    MULTIUSER_TEMP_CHARACTER_ID,
                    'multiuser-receive-character',
                    () => safeStructuredClone(detached),
                    { includeInCharacterOrder: false },
                )
                if (!committed) throw new Error('Failed to persist the multiuser character')
                if (closed) return
                const activated = await dependencies.activateCharacter(MULTIUSER_TEMP_CHARACTER_ID)
                if (closed) return
                if (!activated) throw new Error('Failed to activate the multiuser character')
                ready = true
            })
        },
        receiveChat(incoming) {
            const detached = safeStructuredClone(incoming)
            return enqueue(async () => {
                if (!ready) throw new Error('The multiuser character is not ready')
                let committedChat: Chat | null = null
                const committed = await dependencies.upsertCompleteCharacter(
                    MULTIUSER_TEMP_CHARACTER_ID,
                    'multiuser-receive-chat',
                    (existing) => {
                        if (!existing) {
                            throw new Error('The multiuser character is not available')
                        }
                        const replacement = safeStructuredClone(existing)
                        const chatIndex = replacement.chatPage ?? 0
                        const normalized = normalizeIncomingChatForCurrent(
                            detached,
                            replacement.chats[chatIndex],
                        )
                        replacement.chats[chatIndex] = normalized
                        committedChat = safeStructuredClone(normalized)
                        return replacement
                    },
                    { includeInCharacterOrder: false },
                )
                if (!committed) throw new Error('Failed to persist the multiuser chat')
                if (!closed && committedChat) dependencies.onChatCommitted(committedChat)
            })
        },
        close() {
            if (closed) return
            closed = true
            ready = false
            dependencies.invalidateNavigation()
            dependencies.deselect()
        },
    }
}

let conn:DataConnection
let peer:Peer
let connections:DataConnection[] = []
export let connectionOpen = false
let requestChatSafeQueue = new Map<string, {remaining:number,safe:boolean,conn?:DataConnection}>()
export let ConnectionOpenStore = writable(false)
export let ConnectionIsHost = writable(false)
export let RoomIdStore = writable('')

export async function createMultiuserRoom(){
    //create a room with webrtc
    ConnectionIsHost.set(true)
    alertWait("Loading...")

    const peerJS = await importPeerJS();
    let roomId = v4();
    peer = new peerJS.Peer(
        roomId + "-rmh"
    )

    alertWait("Waiting for peerserver to connect...")
    let open = false
    peer.on('open', function(id) {
        open = true
        roomId = id
    });
    peer.on('connection', function(conn) {
        connections.push(conn)
        console.log("new connection", conn)

        async function requestChar(excludeAssets:string[]|null = null){
            const db = getDatabase({
                snapshot: true
            })
            const selectedCharId = get(selectedCharID)
            const char = safeStructuredClone(db.characters[selectedCharId])
            if(char.type === 'group'){
                return
            }
            char.chats = [char.chats[char.chatPage]]
            conn.send({
                type: 'receive-char',
                data: char
            });
            if(excludeAssets !== null){
                if(char.additionalAssets){
                    const ass = char.additionalAssets.filter((asset) => {
                        return !excludeAssets.includes(asset[1])
                    })

                    for(const a of ass){
                        conn.send({
                            type: 'receive-asset',
                            id: a[1],
                            data: await readImage(a[1])
                        })
                    }
                    
                }
                if(char.emotionImages){
                    const ass = char.emotionImages.filter((asset) => {
                        return !excludeAssets.includes(asset[1])
                    })
                    
                    for(const a of ass){
                        conn.send({
                            type: 'receive-asset',
                            id: a[1],
                            data: await readImage(a[1])
                        })
                    }
                }

            }
        }

        conn.on('data', function(data:ReciveData) {
            if(data.type === 'request-char'){
                requestChar()
            }
            if(data.type === 'receive-char'){
                const db = getDatabase({
                    snapshot: true
                })
                const selectedCharId = get(selectedCharID)
                const char = safeStructuredClone(db.characters[selectedCharId])
                const recivedChar = data.data
                if(char.type === 'group'){
                    return
                }
                char.chats[char.chatPage] = recivedChar.chats[0]
            }
            if(data.type === 'request-chat-sync'){
                const db = getDatabase()
                const selectedCharId = get(selectedCharID)
                const char = db.characters[selectedCharId]
                const normalized = normalizeIncomingChatForCurrent(
                    data.data,
                    char.chats[char.chatPage],
                )
                char.chats[char.chatPage] = normalized
                db.characters[selectedCharId] = char
                latestSyncChat = normalized
                setDatabase(db)

                for(const connection of connections){
                    if(connection.connectionId === conn.connectionId){
                        continue
                    }
                    const rs:ReciveSync = {
                        type: 'receive-chat',
                        data: normalized
                    }
                    connection.send(rs)
                }
            }
            if(data.type === 'request-chat'){
                const db = getDatabase()
                const selectedCharId = get(selectedCharID)
                const char = db.characters[selectedCharId]
                const chat = char.chats[char.chatPage]
                const rs:ReciveSync = {
                    type: 'receive-chat',
                    data: chat
                }
                conn.send(rs)
            }
            if(data.type === 'request-chat-safe'){
                const queue = {
                    remaining: connections.length,
                    safe: true,
                    conn: conn
                }
                requestChatSafeQueue.set(data.id, queue)
                for(const connection of connections){
                    if(connection.connectionId === conn.connectionId){
                        queue.remaining--
                        requestChatSafeQueue.set(data.id, queue)
                        continue
                    }
                    const rs:RequestChatSafe = {
                        type: 'request-chat-safe',
                        id: data.id
                    }
                    connection.send(rs)
                }
                if(queue.remaining === 0){
                    if(waitingMultiuserId === data.id){
                        waitingMultiuserId = ''
                        waitingMultiuserSafe = queue.safe
                    }
                    else if(queue.conn){
                        const rs:ResponseChatSafe = {
                            type: 'response-chat-safe',
                            data: queue.safe,
                            id: data.id
                        }
                        queue.conn.send(rs)
                        requestChatSafeQueue.delete(data.id)
                    }
                }
            }
            if(data.type === 'response-chat-safe'){
                const queue = requestChatSafeQueue.get(data.id)
                if(queue){
                    queue.remaining--
                    if(!data.data){
                        queue.safe = false
                    }
                    if(queue.remaining === 0){
                        if(waitingMultiuserId === data.id){
                            waitingMultiuserId = ''
                            waitingMultiuserSafe = queue.safe
                        }
                        else if(queue.conn){
                            const rs:ResponseChatSafe = {
                                type: 'response-chat-safe',
                                data: queue.safe,
                                id: data.id
                            }
                            queue.conn.send(rs)
                            requestChatSafeQueue.delete(data.id)
                        }
                    }
                }
            }
        });

        conn.on('close', function() {
            for(let i = 0; i < connections.length; i++){
                if(connections[i].connectionId === conn.connectionId){
                    connections.splice(i, 1)
                    break
                }
            }
        })
    });
    while(!open){
        await sleep(100)
    }

    connectionOpen = true
    ConnectionOpenStore.set(true)
    RoomIdStore.set(roomId)
    alertStore.set({
        type: 'none',
        msg: ''
    })
    return
}

let waitingMultiuserId = ''
let waitingMultiuserSafe = false
let latestSyncChat:Chat|null = null

export async function joinMultiuserRoom(){

    //join a room with webrtc
    ConnectionIsHost.set(false)
    alertWait("Loading...")
    const peerJS = await importPeerJS();
    peer = new peerJS.Peer(
        v4() + "-risuai-multiuser-join"
    )

    peer.on('open', async (id) => {
        const roomId = await alertInput("Enter room id")
        alertWait("Waiting for peerserver to connect...")
    
        let open = false
        conn = peer.connect(roomId);
        RoomIdStore.set(roomId)
        const receiveController = createMultiuserReceiveController({
            upsertCompleteCharacter: upsertPersistentCompleteCharacter,
            activateCharacter,
            invalidateNavigation: invalidatePersistentNavigation,
            deselect: () => selectedCharID.set(-1),
            onChatCommitted: (chat) => {
                latestSyncChat = chat
            },
        })

        conn.on('open', function() {
            alertWait("Waiting for host to accept connection")
            open = true
            conn.send({
                type: 'request-char'
            })
        });
        
        conn.on('data', function(data:ReciveData) {
            switch(data.type){
                case 'receive-char':{
                    void receiveController.receiveCharacter(data.data).catch(alertError)
                    break
                }
                case 'receive-asset':{
                    saveImage(data.data, data.id)
                    break
                }
                case 'receive-chat':{
                    void receiveController.receiveChat(data.data).catch(alertError)
                    break
                }
                case 'request-chat-safe':{
                    const rs:ResponseChatSafe = {
                        type: 'response-chat-safe',
                        data: !get(doingChat) || data.id === waitingMultiuserId,
                        id: data.id
                    }
                    conn.send(rs)
                    break
                }
                case 'response-chat-safe':{
                    if(data.id === waitingMultiuserId){
                        waitingMultiuserId = ''
                        waitingMultiuserSafe = data.data
                    }
                }
            }
        });

        conn.on('close', function() {
            receiveController.close()
            alertError("Connection closed")
            connectionOpen = false
            ConnectionOpenStore.set(false)
        })
    
        let waitTime = 0
        while(!open){
            await sleep(100)
            waitTime += 100
            if(waitTime > 10000){
                alertError("Connection timed out")
                return
            }
        }        
        connectionOpen = true
        ConnectionOpenStore.set(true)
        alertNormal("Connected")
    });
    
}


export async function peerSync(){
    if(!connectionOpen){
        return
    }
    await sleep(1)
    const chat = getCurrentChat()
    latestSyncChat = chat
    if(!conn){
        // host user
        for(const connection of connections){
            connection.send({
                type: 'receive-chat',
                data: chat
            });
        }
    }
    else{
        conn.send({
            type: 'request-chat-sync',
            data: chat
        } as RequestSync)
    }
}

export async function peerSafeCheck() {
    if(!connectionOpen){
        return true
    }
    await sleep(500)
    if(!conn){
        waitingMultiuserId = v4()
        requestChatSafeQueue.set(waitingMultiuserId, {
            remaining: connections.length,
            safe: true,
        })
        for(const connection of connections){
            const rs:RequestChatSafe = {
                type: 'request-chat-safe',
                id: waitingMultiuserId
            }
            connection.send(rs)
        }
        while(waitingMultiuserId !== ''){
            await sleep(100)
        }
        return waitingMultiuserSafe
    }
    else{
        waitingMultiuserId = v4()
        const rs:RequestChatSafe = {
            type: 'request-chat-safe',
            id: waitingMultiuserId
        }
        conn.send(rs)
        while(waitingMultiuserId !== ''){
            await sleep(100)
        }
        return waitingMultiuserSafe

    }
}

export function peerRevertChat() {
    if(!connectionOpen || !latestSyncChat){
        return
    }
    setCurrentChat(latestSyncChat)
}
