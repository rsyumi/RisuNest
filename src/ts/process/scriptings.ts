import { userScriptConsole } from '../userScriptConsole'
import { asBuffer } from 'src/ts/util';
import {
    getChatVar,
    getChatVarFromConversation,
    getGlobalChatVar,
    setChatVar,
    setChatVarOnConversation,
} from "../parser/chatVar.svelte";
import { hasher, type simpleCharacterArgument, risuChatParser } from "../parser/parser.svelte";
import type { LuaEngine, LuaFactory } from "wasmoon";
import { getCurrentCharacter, getCurrentChat, getDatabase, setDatabase, type Chat, type Database, type Message, type character, type triggerscript } from "../storage/database.svelte";
import { get } from "svelte/store";
import { DBState, ReloadGUIPointer, selectedCharID } from "../stores.svelte";
import { alertSelect, alertError, alertInput, alertNormal, alertConfirm } from "../alert";
import { HypaProcesser } from "./memory/hypamemory";
import { generateAIImage, type ImageGenerationOptions } from "./stableDiff";
import { writeInlayImage, getInlayAsset } from "./files/inlays";
import type { OpenAIChat, MultiModal } from "./index.svelte";
import { requestChatData, type StreamResponseChunk } from "./request/request";
import { v4 } from "uuid";
import { getModuleLorebooks, getModuleTriggers } from "./modules";
import { Mutex } from "../mutex";
import { tokenize } from "../tokenizer";
import { fetchNative, readImage } from "../globalApi.svelte";
import { loadLoreBookV3PromptFromCompatibilitySnapshot } from './lorebook.svelte';
import { getPersonaPrompt, getUserName, getUserIcon } from '../util';
import { effectiveAuthorNote } from '../authorNotePresets';
import { isTauriMobile } from '../platform';
import { getRuntimePerformanceBudgets, subscribeRuntimePerformanceProfile } from '../runtimePerformanceProfile';
import { createLuaFactory, runLuaSource } from './luaRuntime';
import { LuaWorkerExecutor, LuaWorkerUnavailableError } from './luaWorkerExecutor';
import { createLuaHistoryJSON } from './luaHistoryJSON';
import { luaCodeWrapper } from './luaCodeWrapper';
import { withObjectUrl } from '../objectUrl';
import {
    type ConversationOperationContext,
    type ConversationCommitObserver,
} from './conversationOperationContext';
import {
    captureSelectedConversationAuthority,
    getActiveConversationViewportSource,
    peekActiveConversationSession,
} from '../storage/persistentDataRuntime.svelte';
import type { ActiveConversationSession } from '../storage/activeConversationSession';
import { runSerializedUserTrigger } from './conversationUserTrigger';
import {
    findActiveHistoryWindow,
    getHistoryMessage,
    getHistoryMessageAt,
    historyLength,
    sliceHistory,
    toHistorySpliceIndex,
} from './historyWindowIndex';
import { openHistoryWindowCopy, type HistoryWindowCopy } from './historyWindowWrite';
import { PersistentMutationFencedError } from '../storage/saveCoordinator';
import {
    createScriptConversationAccess,
    type ScriptConversationAccess,
} from './scriptConversationAccess';
import { createScriptDisplayRefreshBatch, rebuildScriptDisplayRow, requestScriptDisplayRefresh } from './scriptDisplayRefresh';
import { beginDisplayScriptRun, noticeScriptTimeout } from './scriptTimeoutNotice';
import {
    displayListenerEngine,
    nextDisplayListenerOrder,
    noteDisplayListenerTimeout,
    skipsDisplayListener,
    type DisplayListenerRequest,
} from './luaDisplayTimeouts';
import { findCharacterById, findCharacterIndexById } from './scriptCharacterLookup';
import { isMetadataOnlySelectedConversation } from '../storage/selectedConversationLifecycle';

/** Deadline for each synchronous Lua segment of a listener. */
const LUA_LISTENER_TIMEOUT_MS = 5_000
/** Deadline for each synchronous Lua segment of a handler the user started. */
const LUA_USER_HANDLER_TIMEOUT_MS = 60_000
let luaFactory:LuaFactory
let luaJSONSource = ''
let luaWorkerUnavailable = false
let luaLocalFactoryPromise: Promise<LuaFactory> | undefined

function canUseLuaWorker() { return typeof Worker !== 'undefined' && !luaWorkerUnavailable }

async function ensureLocalLuaFactory() {
    if (luaFactory) return luaFactory
    const pending = luaLocalFactoryPromise ??= createLuaFactory().then(async factory => {
        await factory.mountFile('json.lua', luaJSONSource)
        luaFactory = factory
        return factory
    })
    try { return await pending }
    finally { if (luaLocalFactoryPromise === pending) luaLocalFactoryPromise = undefined }
}
let ScriptingSafeIds = new Set<string>()
let ScriptingEditDisplayIds = new Set<string>()
let ScriptingLowLevelIds = new Set<string>()
let lastRequestResetTime = 0
let lastRequestsCount = 0

type CapturedCharacterField = 'name'|'desc'|'firstMessage'|'backgroundHTML'

interface CapturedCharacterOwner {
    database: Database
    character: character
    fields: Record<CapturedCharacterField, string|undefined>
}

interface BasicScriptingEngineState {
    code?: string;
    mutex: Mutex;
    cacheKey: string;
    activeUses: number;
    lastUsed: number;
    chat?: Chat;
    setVar?: (key:string, value:string) => boolean|void,
    getVar?: (key:string) => string,
    operationDatabase?: ReturnType<ConversationOperationContext['createDatabaseView']>
    operationCharacter?: character|simpleCharacterArgument
    selectedCharacterId?: string
    capturedCharacterOwner?: CapturedCharacterOwner
    ensureConversationOperation?: () => void
    access?: ScriptConversationAccess
    readChat?: () => Chat
    writeChat?: () => Chat
    readDatabase?: () => Database
    flushVariables?: () => void
    historyJSON?: ReturnType<typeof createLuaHistoryJSON>
    stopSending?: boolean
    displayListenerEngine?: string
}

interface LuaScriptingEngineState extends BasicScriptingEngineState {
    engine?: LuaEngine;
    worker?: LuaWorkerExecutor;
    type: 'lua';
}

interface PythonScriptingEngineState extends BasicScriptingEngineState {
    pyodide?: PyodideContext
    type: 'py';
}

type ScriptingEngineState = LuaScriptingEngineState | PythonScriptingEngineState;

function clearScriptingEngineInvocationState(state: ScriptingEngineState) {
    state.chat = undefined
    state.setVar = undefined
    state.getVar = undefined
    state.operationDatabase = undefined
    state.operationCharacter = undefined
    state.selectedCharacterId = undefined
    state.capturedCharacterOwner = undefined
    state.ensureConversationOperation = undefined
    state.access = undefined
    state.readChat = undefined
    state.writeChat = undefined
    state.readDatabase = undefined
    state.flushVariables = undefined
    state.historyJSON = undefined
    state.stopSending = undefined
}

let ScriptingEngines = new Map<string, ScriptingEngineState>()
const luaDisplayOperationMutexes = new WeakMap<ActiveConversationSession, Mutex>()
let luaFactoryPromise: Promise<void> | null = null;
let scriptingEngineRecency = 0

subscribeRuntimePerformanceProfile(() => {
    evictIdleScriptingEngines()
})

export async function runScripted(code:string, arg:{
    char?:character|simpleCharacterArgument,
    chat?:Chat
    data?: string|OpenAIChat[],
    setVar?: (key:string, value:string) => boolean|void,
    getVar?: (key:string) => string,
    lowLevelAccess?: boolean,
    meta?: object,
    mode?: string,
    type?: 'lua'|'py'
    operationContext?: ConversationOperationContext
    /** The selected conversation through a variable scope and copy-on-edit messages. */
    access?: ScriptConversationAccess
    /** A button or manual trigger the user started, which gets the longer deadline. */
    userStarted?: boolean
    signal?: AbortSignal
    createConversationChat?: () => Chat
    /** An editDisplay request, which skips a listener that timed out for it. */
    displayListener?: DisplayListenerRequest
}){
    const type: 'lua'|'py' = arg.type ?? 'lua'
    if (type === 'py' && isTauriMobile) {
        throw new Error('Python scripting is unavailable on Tauri mobile')
    }
    const char = arg.char ?? getCurrentCharacter()
    const selectedCharacterId = arg.operationContext?.characterId ?? char?.chaId
    const capturedDatabase = DBState.db
    const capturedCharacter = selectedCharacterId === undefined || !capturedDatabase.characters
        ? undefined
        : findCharacterById(capturedDatabase.characters, selectedCharacterId, get(selectedCharID))
    const capturedCharacterOwner: CapturedCharacterOwner|undefined = capturedCharacter
        ? {
            database: capturedDatabase,
            character: capturedCharacter,
            fields: {
                name: capturedCharacter.name,
                desc: capturedCharacter.desc,
                firstMessage: capturedCharacter.firstMessage,
                backgroundHTML: capturedCharacter.backgroundHTML,
            },
        }
        : undefined
    const data = arg.data ?? ''
    const access = arg.access
    const operationDatabase = arg.operationContext?.createDatabaseView(getDatabase())
    const setVar = arg.setVar ?? (access
        ? (key: string, value: string) => access.setVariable(key, value)
        : arg.operationContext
        ? (key: string, value: string) => setChatVarOnConversation(
            arg.operationContext!.chat,
            key,
            value,
        )
        : setChatVar)
    const getVar = arg.getVar ?? (access
        ? (key: string) => access.getVariable(key)
        : arg.operationContext
        ? (key: string) => getChatVarFromConversation(
            operationDatabase!,
            arg.operationContext!.characterId,
            arg.operationContext!.chat,
            key,
        )
        : getChatVar)
    const meta = arg.meta ?? {}
    const mode = arg.mode ?? 'manual'

    let chat = access?.currentChat() ?? arg.operationContext?.chat ?? arg.chat ?? getCurrentChat()
    let lowLevelAccess = arg.lowLevelAccess ?? false

    if(type === 'lua'){
        await ensureLuaFactory()
    }
    const ownerChaId = char?.chaId ?? 'global'
    const functionTimeout = arg.userStarted ? LUA_USER_HANDLER_TIMEOUT_MS : LUA_LISTENER_TIMEOUT_MS
    const ScriptingEngineState = getOrCreateEngineState(mode, type, ownerChaId, code, functionTimeout);
    ScriptingEngineState.activeUses++
    let invocationFinished = false
    const displayListener = mode === 'editDisplay' && type === 'lua' ? arg.displayListener : undefined
    const displayEngine = displayListener
        ? ScriptingEngineState.displayListenerEngine ??= displayListenerEngine(ownerChaId, code)
        : undefined
    
    try {
    return await ScriptingEngineState.mutex.runExclusive(async () => {
        arg.signal?.throwIfAborted()
        if (displayEngine && skipsDisplayListener(displayEngine, displayListener!, data as string)) {
            return { stopSending: false, chat, res: undefined }
        }
        try {
        ScriptingEngineState.chat = chat
        ScriptingEngineState.historyJSON = createLuaHistoryJSON()
        ScriptingEngineState.setVar = setVar
        ScriptingEngineState.getVar = getVar
        ScriptingEngineState.operationCharacter = char
        ScriptingEngineState.operationDatabase = operationDatabase
        ScriptingEngineState.selectedCharacterId = selectedCharacterId
        ScriptingEngineState.capturedCharacterOwner = capturedCharacterOwner
        let invocationChatReady = false
        ScriptingEngineState.ensureConversationOperation = () => {
            if (!arg.createConversationChat || invocationChatReady) return
            const ownedChat = arg.createConversationChat()
            invocationChatReady = true
            ScriptingEngineState.chat = ownedChat
            ScriptingEngineState.setVar = (key: string, value: string) =>
                setChatVarOnConversation(ownedChat, key, value)
            ScriptingEngineState.getVar = (key: string) =>
                getChatVarFromConversation(capturedDatabase, selectedCharacterId, ownedChat, key)
        }
        const legacyChat = () => {
            ScriptingEngineState.ensureConversationOperation?.()
            return ScriptingEngineState.chat
        }
        ScriptingEngineState.access = access
        ScriptingEngineState.readChat = access ? () => access.readChat() : legacyChat
        ScriptingEngineState.writeChat = access ? () => access.writeChat() : legacyChat
        ScriptingEngineState.readDatabase = access
            ? () => {
                access.readChat()
                return access.databaseView(getDatabase())
            }
            : () => {
                ScriptingEngineState.ensureConversationOperation?.()
                return ScriptingEngineState.operationDatabase ?? getDatabase()
            }
        ScriptingEngineState.flushVariables = access ? () => { access.flushVariables() } : undefined
        ScriptingEngineState.stopSending = false
        if (code !== ScriptingEngineState.code || (ScriptingEngineState.type === 'lua' && ScriptingEngineState.worker?.isClosed())) {
            try {
            let declareAPI:(name: string, func:Function) => void
            const luaCallbacks = new Map<string, Function>()

            if(ScriptingEngineState.type === 'lua'){
                console.log('Creating new Lua engine for mode:', mode)
                ScriptingEngineState.engine?.global.close()
                ScriptingEngineState.worker?.close()
                ScriptingEngineState.worker = undefined
                if (canUseLuaWorker()) {
                    try { ScriptingEngineState.worker = new LuaWorkerExecutor(functionTimeout, createScriptDisplayRefreshBatch()) }
                    catch (error) {
                        if (!(error instanceof LuaWorkerUnavailableError)) throw error
                        luaWorkerUnavailable = true
                    }
                }
                if (!ScriptingEngineState.worker) {
                    ScriptingEngineState.engine = await (await ensureLocalLuaFactory()).createEngine({
                        injectObjects: true,
                        enableProxy: false,
                        functionTimeout,
                    })
                }
                declareAPI = (name: string, func: Function) => {
                    luaCallbacks.set(name, func)
                    if (ScriptingEngineState.worker) ScriptingEngineState.worker.register(name, func)
                    else ScriptingEngineState.engine.global.set(name, func)
                }
            }
            if(ScriptingEngineState.type === 'py'){
                console.log('Creating new Pyodide context for mode:', mode)
                ScriptingEngineState.pyodide?.close()
                ScriptingEngineState.pyodide = new PyodideContext()
                declareAPI = (name:string, func:Function) => {
                    ScriptingEngineState.pyodide?.declareAPI(name, func as any)
                }
            }
            const ensureScriptingVariables = () => {
                if (!ScriptingEngineState.access) {
                    ScriptingEngineState.ensureConversationOperation?.()
                }
            }
            const readScriptingChat = () => ScriptingEngineState.readChat()
            const writeScriptingChat = () => ScriptingEngineState.writeChat()
            const editScriptingMessage = (index: number, edit: (message: Message) => void) => {
                if (ScriptingEngineState.access) {
                    ScriptingEngineState.access.editMessage(index, edit)
                    return
                }
                const chat = writeScriptingChat()
                const message = chat.message
                    ? getHistoryMessageAt(chat, index)
                    : undefined
                if (message) edit(message)
            }
            // Commits buffered variables before a call that waits, so triggers and displays running meanwhile read them.
            const flushScriptingVariables = () => ScriptingEngineState.flushVariables?.()
            const getScriptingDatabase = () => ScriptingEngineState.readDatabase?.() ?? getDatabase()
            const getScriptingCharacterIndex = (db: Database) => {
                if (ScriptingEngineState.selectedCharacterId !== undefined) {
                    return findCharacterIndexById(
                        db.characters,
                        ScriptingEngineState.selectedCharacterId,
                        get(selectedCharID),
                    )
                }
                return get(selectedCharID)
            }
            const getScriptingCharacter = () => {
                const db = getScriptingDatabase()
                return db.characters[getScriptingCharacterIndex(db)]
            }
            const updateCapturedCharacterField = (
                field: CapturedCharacterField,
                value: string,
            ) => {
                const owner = ScriptingEngineState.capturedCharacterOwner
                if (!owner || DBState.db !== owner.database) return false

                const liveCharacter = DBState.db.characters?.find(
                    (candidate) => candidate.chaId === owner.character.chaId,
                )
                if (liveCharacter !== owner.character) return false

                const liveRecord = liveCharacter as unknown as Record<
                    CapturedCharacterField,
                    string|undefined
                >
                if (!Object.is(liveRecord[field], owner.fields[field])) {
                    return false
                }

                liveRecord[field] = value
                owner.fields[field] = value
                const projectedCharacter = ScriptingEngineState.operationDatabase
                    ?.characters.find(
                        (candidate) => candidate.chaId === owner.character.chaId,
                    )
                if (projectedCharacter && projectedCharacter !== liveCharacter) {
                    const projectedRecord = projectedCharacter as unknown as Record<
                        CapturedCharacterField,
                        string|undefined
                    >
                    projectedRecord[field] = value
                }
                return true
            }
            declareAPI('getChatVar', (id:string,key:string) => {
                ensureScriptingVariables()
                return ScriptingEngineState.getVar(key)
            })
            declareAPI('setChatVar', (id:string,key:string, value:string) => {
                if(!ScriptingSafeIds.has(id) && !ScriptingEditDisplayIds.has(id)){
                    return
                }
                ensureScriptingVariables()
                ScriptingEngineState.setVar(key, value)
            })
            declareAPI('setChatVarChanged', (id:string,key:string, value:string) => {
                if(!ScriptingSafeIds.has(id) && !ScriptingEditDisplayIds.has(id)){
                    return
                }
                ensureScriptingVariables()
                if(ScriptingEngineState.setVar(key, value) === true){
                    return true
                }
            })
            declareAPI('getGlobalVar', (id:string, key:string) => {
                return getGlobalChatVar(key)
            })
            declareAPI('stopChat', (id:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                ScriptingEngineState.stopSending = true
            })
            declareAPI('alertError', (id:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                alertError(value)
            })
            declareAPI('alertNormal', (id:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                alertNormal(value)
            })
            declareAPI('alertInput', (id:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                flushScriptingVariables()
                return alertInput(value)
            })
            declareAPI('alertSelect', (id:string, value:string[]) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                flushScriptingVariables()
                return alertSelect(value)
            })
            declareAPI('alertConfirm', (id:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                flushScriptingVariables()
                return alertConfirm(value).then(res => res ? true : false)
            })

            declareAPI('getChatMain', (id:string, index:number) => {
                const chat = getHistoryMessageAt(readScriptingChat(), index)
                if(!chat){
                    return JSON.stringify(null)
                }
                const data = {
                    role: chat.role,
                    data: chat.data,
                    time: chat.time ?? 0
                }
                return JSON.stringify(data)
            })

            declareAPI('getChatData', (id:string, index:number) => {
                const chat = getHistoryMessageAt(readScriptingChat(), index)
                return chat?.data ?? ''
            })

            declareAPI('getChatRole', (id:string, index:number) => {
                const chat = getHistoryMessageAt(readScriptingChat(), index)
                return chat?.role ?? ''
            })

            declareAPI('getRecentChatsMain', (id:string, count:number) => {
                const chats = readScriptingChat().message
                const safeCount = Math.max(0, Math.floor(count || 0))
                const start = Math.max(0, chats.length - safeCount)
                return JSON.stringify(chats.slice(start).map((v) => ({
                    role: v.role,
                    data: v.data,
                    time: v.time ?? 0,
                })))
            })

            declareAPI('setChat', (id:string, index:number, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                editScriptingMessage(index, (message) => {
                    message.data = value ?? ''
                })
            })
            declareAPI('setChatRole', (id:string, index:number, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                editScriptingMessage(index, (message) => {
                    message.role = value === 'user' ? 'user' : 'char'
                })
            })
            declareAPI('cutChat', (id:string, start:number, end:number) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                const chat = writeScriptingChat()
                chat.message = sliceHistory(chat, start, end)
            })
            declareAPI('removeChat', (id:string, index:number) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                const chat = writeScriptingChat()
                const start = toHistorySpliceIndex(chat, index)
                if (start === null) return
                chat.message.splice(start, 1)
            })
            declareAPI('addChat', (id:string, role:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                let roleData:'user'|'char' = role === 'user' ? 'user' : 'char'
                const message = {role: roleData, data: value ?? '', chatId: v4()}
                if (ScriptingEngineState.access) {
                    ScriptingEngineState.access.appendMessage(message)
                    return
                }
                writeScriptingChat().message.push(message)
            })
            declareAPI('insertChat', (id:string, index:number, role:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                const chat = writeScriptingChat()
                let roleData:'user'|'char' = role === 'user' ? 'user' : 'char'
                const start = toHistorySpliceIndex(chat, index)
                if (start === null) return
                chat.message.splice(start, 0, {role: roleData, data: value ?? '', chatId: v4()})
            })

            declareAPI('getTokens', async (id:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                return await tokenize(value)
            })

            declareAPI('getChatLength', (id:string) => {
                return historyLength(readScriptingChat())
            })

            declareAPI('getFullChatMain', (id:string) => {
                return ScriptingEngineState.historyJSON(readScriptingChat().message)
            })

            declareAPI('sleep', (id:string, time:number) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                flushScriptingVariables()
                return new Promise((resolve) => {
                    setTimeout(() => {
                        resolve(true)
                    }, time)
                })
            })

            declareAPI('cbs', (value) => {
                const database = getScriptingDatabase()
                const operationCharacter = ScriptingEngineState.operationCharacter
                const parserCharacter = operationCharacter?.type === 'simple'
                    ? getScriptingCharacter()
                    : operationCharacter ?? getScriptingCharacter()
                return risuChatParser(value, {
                    chara: parserCharacter,
                    db: ScriptingEngineState.access ? database : ScriptingEngineState.operationDatabase,
                    selectedCharacterId: ScriptingEngineState.selectedCharacterId,
                    getChatVar: ScriptingEngineState.getVar,
                    setChatVar: ScriptingEngineState.setVar,
                })
            })
            
            declareAPI('setFullChatMain', (id:string, value:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                const chat = writeScriptingChat()
                const realValue = JSON.parse(value)
                const originals = new Map<string, { count: number, message: Chat['message'][number] | null }>()
                for (const message of chat.message) {
                    if (!message.chatId) continue
                    const entry = originals.get(message.chatId)
                    if (entry) {
                        entry.count += 1
                        entry.message = null
                    } else originals.set(message.chatId, { count: 1, message })
                }
                const counts = new Map<string, number>()
                for (const message of realValue) {
                    if (typeof message.chatId === 'string')
                        counts.set(message.chatId, (counts.get(message.chatId) ?? 0) + 1)
                }
                const kept = new Map<string, number>()
                chat.message = realValue.map((v) => {
                    // A stored chatId is kept for as many messages as the chat holds it, in order;
                    // further copies and unknown ids become new messages, never another output
                    // target. Plugin metadata carries over only when the id names one message on both sides.
                    const entry = typeof v.chatId === 'string' ? originals.get(v.chatId) : undefined
                    const used = entry ? kept.get(v.chatId) ?? 0 : 0
                    if (!entry || used >= entry.count) return { role: v.role, data: v.data, chatId: v4() }
                    kept.set(v.chatId, used + 1)
                    const original = counts.get(v.chatId) === 1 ? entry.message : null
                    return {
                        ...original,
                        role: v.role,
                        data: v.data,
                        chatId: v.chatId,
                    }
                })
            })

            declareAPI('logMain', (value:string) => {
                userScriptConsole.log(JSON.parse(value))
            })

            declareAPI('reloadDisplay', (id:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                flushScriptingVariables()
                requestScriptDisplayRefresh({ all: true })
            })

            declareAPI('reloadChat', (id: string, index: number) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                flushScriptingVariables()
                rebuildScriptDisplayRow(index)
            })

            //Low Level Access
            declareAPI('similarity', async (id:string, source:string, value:string[]) => {
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }
                flushScriptingVariables()
                const processer = new HypaProcesser()
                await processer.addText(value)
                return await processer.similaritySearch(source)
            })

            declareAPI('request', async (id:string, url:string) => {
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }
                flushScriptingVariables()

                if(lastRequestResetTime + 60000 < Date.now()){
                    lastRequestsCount = 0
                    lastRequestResetTime = Date.now()
                }
                
                if(lastRequestsCount > 5){
                    return JSON.stringify({
                        status: 429,
                        data: 'Too many requests. you can request 5 times per minute'
                    })
                }

                lastRequestsCount++

                try {
                    //for security and other reasons, only get request in 120 char is allowed
                    if(url.length > 120){
                        return JSON.stringify({
                            status: 413,
                            data: 'URL to large. max is 120 characters'
                        })
                    }

                    if(!url.startsWith('https://')){
                        return JSON.stringify({
                            status: 400,
                            data: "Only https requests are allowed"
                        })
                    }

                    const bannedURL = [
                        "https://realm.risuai.net",
                        "https://risuai.net",
                        "https://risuai.xyz"
                    ]

                    for(const burl of bannedURL){

                        if(url.startsWith(burl)){
                            return JSON.stringify({
                                status: 400,
                                data: "request to " + url + ' is not allowed'
                            })
                        }
                    }

                    //browser fetch
                    const d = await fetchNative(url, {
                        method: "GET"
                    })
                    const text = await d.text()
                    return JSON.stringify({
                        status: d.status,
                        data: text
                    })

                } catch (error) {
                    return JSON.stringify({
                        status: 400,
                        data: 'internal error'
                    })
                }
            })

            declareAPI('generateImage', async (id:string, value:string, negValue:string = '', options?:ImageGenerationOptions) => {
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }
                flushScriptingVariables()
                const orientation = options?.orientation
                if(orientation !== undefined && orientation !== 'landscape' && orientation !== 'portrait'){
                    return 'Error: Image orientation must be landscape or portrait, received ' + orientation
                }

                const gen = await generateAIImage(
                    value,
                    ScriptingEngineState.operationCharacter as character,
                    negValue,
                    'inlay',
                    options,
                )
                if(!gen){
                    return 'Error: Image generation failed'
                }
                const imgHTML = new Image()
                imgHTML.src = gen
                const inlay = await writeInlayImage(imgHTML)
                return `{{inlay::${inlay}}}`
            })

            declareAPI('getCharacterImageMain', async (id:string) => {
                try {
                    const db = getScriptingDatabase()
                    const selectedChar = getScriptingCharacterIndex(db)

                    if (selectedChar < 0 || selectedChar >= db.characters.length) {
                        return ''
                    }

                    const character = db.characters[selectedChar]
                    
                    if (!character || !character.image) {
                        return ''
                    }
                    
                    const img = await readImage(character.image)
                    const imgObj = new Image()
                    const extention = character.image.split('.').at(-1)

                    const imgid = await withObjectUrl(
                        new Blob([asBuffer(img)], {type: `image/${extention}`}),
                        (url) => writeInlayImage(
                            imgObj,
                            { name: character.image, ext: extention, id: character.image },
                            url,
                        ),
                    )

                    if (imgid) {
                        return `{{inlayed::${imgid}}}`
                    }
                    console.warn('Failed to create character image inlay')
                    return ''
                } catch (error) {
                    console.error('Error in getCharacterImageMain:', error)
                    return ''
                }
            })

            declareAPI('getPersonaImageMain', async (id:string) => {
                try {
                    const icon = getUserIcon()

                    if(!icon) {
                        return ''
                    }

                    const img = await readImage(icon)
                    const imgObj = new Image()
                    const extention = icon.split('.').at(-1)

                    const imgid = await withObjectUrl(
                        new Blob([asBuffer(img)], {type: `image/${extention}`}),
                        (url) => writeInlayImage(imgObj, { name: icon, ext: extention, id: icon }, url),
                    )

                    if (imgid) {
                        return `{{inlayed::${imgid}}}`
                    }
                    
                    console.warn('Failed to create character image inlay')
                    return ''
                } catch (error) {
                    console.error('Error in getCharacterImageMain:', error)
                    return ''
                }
            })

            declareAPI('hash', async (id:string, value:string) => {
                return await hasher(new TextEncoder().encode(value))
            })

            const parseLuaOptions = (optionsStr?: string) => {
                if (!optionsStr) {
                    return {};
                }

                try {
                    const parsed = JSON.parse(optionsStr);
                    return parsed && typeof parsed === 'object' ? parsed : {};
                } catch {
                    return {};
                }
            };

            const collectLuaStreamText = async (stream: ReadableStream<StreamResponseChunk>) => {
                const reader = stream.getReader();
                let text = '';

                try {
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) {
                            break;
                        }
                        if (value && typeof value['0'] === 'string') {
                            text = value['0'];
                        }
                    }
                } finally {
                    reader.releaseLock();
                }

                return text;
            };

            declareAPI('LLMMain', async (id:string, promptStr:string, useMultimodal: boolean = false, optionsStr: string = '') => {
                let prompt:{
                    role: string,
                    content: string
                }[] = JSON.parse(promptStr)
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }
                flushScriptingVariables()
                let promptbody:OpenAIChat[] = prompt.map((dict) => {
                    let role:'system'|'user'|'assistant' = 'assistant'
                    switch(dict['role']){
                        case 'system':
                        case 'sys':
                            role = 'system'
                            break
                        case 'user':
                            role = 'user'
                            break
                        case 'assistant':
                        case 'bot':
                        case 'char':{
                            role = 'assistant'
                            break
                        }
                    }

                    return {
                        content: dict['content'] ?? '',
                        role: role,
                    }
                })

                if(useMultimodal) {
                    for(const msg of promptbody) {
                        const inlays:string[] = []
                        msg.content = msg.content.replace(/{{(inlay|inlayed|inlayeddata)::(.+?)}}/g, (
                            match: string,
                            p1: string,
                            p2: string
                        ) => {
                            if(msg.role === 'assistant') {
                                if(p2 && p1 === 'inlayeddata') {
                                    inlays.push(p2)
                                }
                            }
                            else {
                                if(p2) {
                                    inlays.push(p2)
                                }
                            }
                            return ''
                        })
                        
                        const multimodals: MultiModal[] = []
                        for(const inlay of inlays) {
                            const inlayData = await getInlayAsset(inlay)
                            multimodals.push({
                                type: inlayData?.type,
                                base64: inlayData?.data,
                                width: inlayData?.width,
                                height: inlayData?.height
                            })
                        }

                        msg.multimodals = multimodals.length > 0 ? multimodals : undefined
                    }
                }

                const options = parseLuaOptions(optionsStr) as { streaming?: boolean }
                const result = await requestChatData({
                    formated: promptbody,
                    bias: {},
                    useStreaming: options.streaming === true,
                    forceStreaming: options.streaming === true,
                    noMultiGen: true,
                }, 'model')

                if(result.type === 'fail'){
                    return JSON.stringify({
                        success: false,
                        result: 'Error: ' + result.result
                    })
                }

                if(result.type === 'streaming'){
                    try {
                        return JSON.stringify({
                            success: true,
                            result: await collectLuaStreamText(result.result)
                        })
                    } catch (error) {
                        return JSON.stringify({
                            success: false,
                            result: 'Error: ' + error
                        })
                    }
                }

                if(result.type === 'multiline'){
                    return JSON.stringify({
                        success: false,
                        result: result.result
                    })
                }

                return JSON.stringify({
                    success: true,
                    result: result.result
                })
            })

            declareAPI('simpleLLM', async (id:string, prompt:string) => {
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }
                flushScriptingVariables()
                const result = await requestChatData({
                    formated: [{
                        role: 'user',
                        content: prompt
                    }],
                    bias: {},
                    useStreaming: false,
                    noMultiGen: true,
                }, 'model')

                if(result.type === 'fail'){
                    return {
                        success: false,
                        result: 'Error: ' + result.result
                    }
                }

                if(result.type === 'streaming' || result.type === 'multiline'){
                    return {
                        success: false,
                        result: result.result
                    }
                }

                return {
                    success: true,
                    result: result.result
                }
            })
            
            declareAPI('getName', (id:string) => {
                const db = getScriptingDatabase()
                const selectedChar = getScriptingCharacterIndex(db)
                const char = db.characters[selectedChar]
                return char.name
            })

            declareAPI('setName', (id:string, name:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                if(typeof name !== 'string'){
                    throw('Invalid data type')
                }
                updateCapturedCharacterField('name', name)
            })

            declareAPI('getDescription', (id:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                const selectedChar = getScriptingCharacterIndex(DBState.db)
                const char = DBState.db.characters[selectedChar]

                return char.desc
            })

            declareAPI('setDescription', (id:string, desc:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                if(typeof desc !== 'string'){
                    throw('Invalid data type')
                }

                updateCapturedCharacterField('desc', desc)
            })

            declareAPI('getCharacterFirstMessage', (id:string) => {
                const selectedChar = getScriptingCharacterIndex(DBState.db)
                const char = DBState.db.characters[selectedChar]
                return char.firstMessage
            })

            declareAPI('setCharacterFirstMessage', (id:string, data:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                if(typeof data !== 'string'){
                    return false
                }
                return updateCapturedCharacterField('firstMessage', data)
            })

            declareAPI('getPersonaName', (id:string) => {
                return getUserName()
            })

            declareAPI('getPersonaDescription', (id:string) => {
                const db = getScriptingDatabase()
                const selectedChar = getScriptingCharacterIndex(db)
                const char = db.characters[selectedChar]

                return risuChatParser(getPersonaPrompt(), { chara: char })
            })

            declareAPI('getAuthorsNote', (id:string) => {
                const chat = readScriptingChat()
                return chat ? effectiveAuthorNote(getScriptingDatabase().authorNotePresets, chat) : ''
            })

            declareAPI('getBackgroundEmbedding', (id:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                const db = getScriptingDatabase()
                const selectedChar = getScriptingCharacterIndex(db)
                const char = db.characters[selectedChar]
                return char.backgroundHTML
            })

            declareAPI('setBackgroundEmbedding', (id:string, data:string) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }
                if(typeof data !== 'string'){
                    return false
                }
                return updateCapturedCharacterField('backgroundHTML', data)
            })

            // Lore books
            declareAPI('getLoreBooksMain', (id:string, search:string) => {
                const db = getScriptingDatabase()
                const selectedChar = db.characters[getScriptingCharacterIndex(db)]


                const loreSources = [
                    selectedChar.chats[selectedChar.chatPage]?.localLore ?? [],
                    selectedChar.globalLore,
                    getModuleLorebooks()
                ]

                const found = []
                for (const source of loreSources) {
                    for (const b of source) {
                        if (b.comment === search) {
                            found.push({ ...b, content: risuChatParser(b.content, {
                                chara: selectedChar,
                                db,
                                selectedCharacterId: ScriptingEngineState.selectedCharacterId,
                                getChatVar: ScriptingEngineState.getVar,
                                setChatVar: ScriptingEngineState.setVar,
                            }) })
                        }
                    }
                }

                return JSON.stringify(found)
            })

            type upsertLoreBookOptions = {
                alwaysActive?: boolean
                insertOrder?: number
                key?: string
                secondKey?: string
                regex?: boolean
            }

            declareAPI('upsertLocalLoreBook', (id:string, name:string, content:string, options:upsertLoreBookOptions) => {
                if(!ScriptingSafeIds.has(id)){
                    return
                }

                if (!ScriptingEngineState.operationCharacter) {
                    return
                }

                const currentChat = writeScriptingChat()

                const {
                    alwaysActive = false,
                    insertOrder = 100,
                    key = '',
                    regex = false,
                    secondKey = '',
                } = options

                if (!currentChat) {
                    return
                }

                const newLocalLoreBooks = (currentChat.localLore ?? [])
                    .filter((book) => book.comment !== name)
                newLocalLoreBooks.push({
                    alwaysActive,
                    comment: name,
                    content: content,
                    insertorder: insertOrder,
                    mode: 'normal',
                    key,
                    secondkey: secondKey,
                    selective: !!secondKey,
                    useRegex: regex,
                })
                currentChat.localLore = newLocalLoreBooks
            })

            declareAPI('loadLoreBooksMain', async (id:string, reserve:number) => {
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }

                const db = getScriptingDatabase()

                const selectedChar = db.characters[getScriptingCharacterIndex(db)]



                const fullLoreBooks = (await loadLoreBookV3PromptFromCompatibilitySnapshot()).actives
                const maxContext = db.maxContext - reserve
                if (maxContext < 0) {
                    return JSON.stringify([])
                }

                let totalTokens = 0
                const loreBooks = []

                for (const book of fullLoreBooks) {
                    const parsed = risuChatParser(book.prompt, { chara: selectedChar }).trim()
                    if (parsed.length === 0) {
                        continue
                    }

                    const tokens = await tokenize(parsed)

                    if (totalTokens + tokens > maxContext) {
                        break
                    }
                    totalTokens += tokens
                    loreBooks.push({
                        data: parsed,
                        role: book.role === 'assistant' ? 'char' : book.role,
                    })
                }

                return JSON.stringify(loreBooks)
            })

            declareAPI('axLLMMain', async (id:string, promptStr:string, useMultimodal: boolean = false, optionsStr: string = '') => {
                let prompt:{
                    role: string,
                    content: string
                }[] = JSON.parse(promptStr)
                if(!ScriptingLowLevelIds.has(id)){
                    return
                }
                flushScriptingVariables()
                let promptbody:OpenAIChat[] = prompt.map((dict) => {
                    let role:'system'|'user'|'assistant' = 'assistant'
                    switch(dict['role']){
                        case 'system':
                        case 'sys':
                            role = 'system'
                            break
                        case 'user':
                            role = 'user'
                            break
                        case 'assistant':
                        case 'bot':
                        case 'char':{
                            role = 'assistant'
                            break
                        }
                    }

                    return {
                        content: dict['content'] ?? '',
                        role: role,
                    }
                })

                if(useMultimodal) {
                    for(const msg of promptbody) {
                        const inlays:string[] = []
                        msg.content = msg.content.replace(/{{(inlay|inlayed|inlayeddata)::(.+?)}}/g, (
                            match: string,
                            p1: string,
                            p2: string
                        ) => {
                            if(msg.role === 'assistant') {
                                if(p2 && p1 === 'inlayeddata') {
                                    inlays.push(p2)
                                }
                            }
                            else {
                                if(p2) {
                                    inlays.push(p2)
                                }
                            }
                            return ''
                        })
                        
                        const multimodals: MultiModal[] = []
                        for(const inlay of inlays) {
                            const inlayData = await getInlayAsset(inlay)
                            multimodals.push({
                                type: inlayData?.type,
                                base64: inlayData?.data,
                                width: inlayData?.width,
                                height: inlayData?.height
                            })
                        }

                        msg.multimodals = multimodals.length > 0 ? multimodals : undefined
                    }
                }

                const options = parseLuaOptions(optionsStr) as { mode?: string, streaming?: boolean }
                const modes = new Set(['emotion', 'memory', 'otherAx', 'submodel', 'translate'])
                const mode = options.mode ?? 'otherAx'
                if (!modes.has(mode)) {
                    return JSON.stringify({
                        result: 'Error: Invalid axLLM mode: ' + mode,
                        success: false
                    })
                }
                const result = await requestChatData({
                    formated: promptbody,
                    bias: {},
                    useStreaming: options.streaming === true,
                    forceStreaming: options.streaming === true,
                    noMultiGen: true,
                }, mode as 'emotion' | 'memory' | 'otherAx' | 'submodel' | 'translate')

                if(result.type === 'fail'){
                    return JSON.stringify({
                        success: false,
                        result: 'Error: ' + result.result
                    })
                }

                if(result.type === 'streaming'){
                    try {
                        return JSON.stringify({
                            success: true,
                            result: await collectLuaStreamText(result.result)
                        })
                    } catch (error) {
                        return JSON.stringify({
                            success: false,
                            result: 'Error: ' + error
                        })
                    }
                }

                if(result.type === 'multiline'){
                    return JSON.stringify({
                        success: false,
                        result: result.result
                    })
                }

                return JSON.stringify({
                    success: true,
                    result: result.result
                })
            })

            declareAPI('getCharacterLastMessage', (id: string) => {
                const chat = readScriptingChat()
                if (!chat) {
                    return ''
                }

                const selchar = getScriptingCharacter()

                let pointer = chat.message.length - 1
                while (pointer >= 0) {
                    if (chat.message[pointer].role === 'char') {
                        const messageData = chat.message[pointer].data
                        return messageData
                    }
                    pointer--
                }

                return selchar?.firstMessage ?? ''
            })

            declareAPI('getUserLastMessage', (id: string) => {
                const chat = readScriptingChat()
                if (!chat) {
                    return ''
                }

                let pointer = chat.message.length - 1
                while (pointer >= 0) {
                    if (chat.message[pointer].role === 'user') {
                        const messageData = chat.message[pointer].data
                        return messageData
                    }
                    pointer--
                }

                return ''
            })

            declareAPI('getCharacterLastMessage', (id: string) => {
                const chat = readScriptingChat()
                if (!chat) {
                    return ''
                }

                const selchar = getScriptingCharacter()

                let pointer = chat.message.length - 1
                while (pointer >= 0) {
                    if (chat.message[pointer].role === 'char') {
                        const messageData = chat.message[pointer].data
                        return messageData
                    }
                    pointer--
                }

                return selchar?.firstMessage ?? ''
            })

            declareAPI('getUserLastMessage', (id: string) => {
                const chat = readScriptingChat()
                if (!chat) {
                    return ''
                }

                let pointer = chat.message.length - 1
                while (pointer >= 0) {
                    if (chat.message[pointer].role === 'user') {
                        const messageData = chat.message[pointer].data
                        return messageData
                    }
                    pointer--
                }
                return ''
            })

            console.log('Running Lua code:', code)
            if(ScriptingEngineState.type === 'lua'){
                if (ScriptingEngineState.worker) {
                    try { await ScriptingEngineState.worker.initialize(luaCodeWrapper(code), luaJSONSource, arg.signal) }
                    catch (error) {
                        if (!(error instanceof LuaWorkerUnavailableError)) throw error
                        luaWorkerUnavailable = true
                        ScriptingEngineState.worker.close()
                        ScriptingEngineState.worker = undefined
                        arg.signal?.throwIfAborted()
                        const engine = await (await ensureLocalLuaFactory()).createEngine({
                            injectObjects: true, enableProxy: false, functionTimeout,
                        })
                        ScriptingEngineState.engine = engine
                        for (const [name, func] of luaCallbacks) engine.global.set(name, func)
                        await runLuaSource(engine, luaCodeWrapper(code))
                    }
                } else await runLuaSource(ScriptingEngineState.engine, luaCodeWrapper(code))
            }
            if(ScriptingEngineState.type === 'py'){
                await ScriptingEngineState.pyodide?.init(code)
            }
            ScriptingEngineState.code = code
            } catch (error) {
                discardFailedScriptingEngineState(ScriptingEngineState)
                // Top-level code that times out would spend the same budget again for every queued row.
                if (displayEngine && isLuaTimeoutError(error)) noteDisplayListenerTimeout(displayEngine, displayListener!, data as string)
                throw error
            }
        }
        let accessKey = v4()
        if(mode === 'editDisplay'){
            ScriptingEditDisplayIds.add(accessKey)
        }
        else{
            ScriptingSafeIds.add(accessKey)
            if(lowLevelAccess){
                ScriptingLowLevelIds.add(accessKey)
            }
        }
        try {
        let res:any
        if(ScriptingEngineState.type === 'lua'){
            const luaEngine = ScriptingEngineState.engine
            const worker = ScriptingEngineState.worker
            const getFunction = (name: string) => worker
                ? (...args: unknown[]) => worker.invoke(name, args, arg.signal)
                : luaEngine.global.get(name)
            const listenerStartedAt = performance.now()
            try {
                switch(mode){
                    case 'input':{
                        const func = getFunction('onInput')
                        if(func){
                            res = await func(accessKey)
                        }
                        break
                    }
                    case 'output':{
                        const func = getFunction('onOutput')
                        if(func){
                            res = await func(accessKey)
                        }
                        break
                    }
                    case 'start':{
                        const func = getFunction('onStart')
                        if(func){
                            res = await func(accessKey)
                        }
                        break
                    }
                    case 'onButtonClick':{
                        const func = getFunction('onButtonClick')
                        if(func){
                            res = await func(accessKey, data)
                        }
                        break
                    }
                    case 'onFormSubmit':{
                        const func = getFunction('onFormSubmit')
                        if(func){
                            res = await func(accessKey, data, JSON.stringify(meta))
                        }
                        break
                    }
                    case 'editRequest':
                    case 'editDisplay':
                    case 'editInput':
                    case 'editOutput':{
                        const func = getFunction('callListenMain')
                        if(func){
                            // editDisplay takes a raw string, and must output a raw string
                            const directString = mode === 'editDisplay'
                            const value = directString ? data : JSON.stringify(data)
                            res = await func(mode, accessKey, value, JSON.stringify(meta))
                            if (!directString) {
                                res = JSON.parse(res)
                            }
                        }
                        break
                    }
                    default:{
                        const func = getFunction(mode)
                        if(func){
                            res = await func(accessKey)
                        }
                        break
                    }
                }   
                if(res === false){
                    ScriptingEngineState.stopSending = true
                }
            } catch (error) {
                if (worker?.isClosed()) discardFailedScriptingEngineState(ScriptingEngineState)
                arg.signal?.throwIfAborted()
                if (isLuaTimeoutError(error)) {
                    const diagnosticMode = ['input', 'output', 'start', 'onButtonClick', 'onFormSubmit', 'editRequest',
                        'editDisplay', 'editInput', 'editOutput', 'manual'].includes(mode) ? mode : 'custom'
                    console.error({ code: 'lua-listener-timeout', mode: diagnosticMode,
                        elapsedMs: performance.now() - listenerStartedAt })
                    if (displayEngine) noteDisplayListenerTimeout(displayEngine, displayListener!, data as string)
                    noticeScriptTimeout(mode === 'editDisplay')
                } else {
                    console.error(error)
                }
            }
        }
        if(ScriptingEngineState.type === 'py'){
            switch(mode){
                case 'input':{
                    res = await ScriptingEngineState.pyodide?.python(`onInput('${accessKey}')`)
                    break
                }
                case 'output':{
                    res = await ScriptingEngineState.pyodide?.python(`onOutput('${accessKey}')`)
                    break
                }
                case 'start':{
                    res = await ScriptingEngineState.pyodide?.python(`onStart('${accessKey}')`)
                    break
                }
                case 'onButtonClick':{
                    res = await ScriptingEngineState.pyodide?.python(`onButtonClick('${accessKey}', '${data as string}')`)
                    break
                }
                case 'editRequest':
                case 'editDisplay':
                case 'editInput':
                case 'editOutput':{
                    res = await ScriptingEngineState.pyodide?.python(`callListenMain('${mode}', '${accessKey}', '${JSON.stringify(data)}', '${JSON.stringify(meta)}')`)
                    res = JSON.parse(res)
                    break
                }
                default:{
                    res = await ScriptingEngineState.pyodide?.python(`${mode}('${accessKey}')`)
                    break
                }
            }
        }
        chat = access?.currentChat() ?? ScriptingEngineState.chat

        return {
            stopSending: ScriptingEngineState.stopSending ?? false, chat, res
        }
        } finally {
            ScriptingSafeIds.delete(accessKey)
            ScriptingLowLevelIds.delete(accessKey)
            ScriptingEditDisplayIds.delete(accessKey)
            invocationFinished = true
            finishScriptingEngineUse(ScriptingEngineState)
        }
        } finally {
            clearScriptingEngineInvocationState(ScriptingEngineState)
        }
    })
    } finally {
        if (!invocationFinished) {
            finishScriptingEngineUse(ScriptingEngineState)
        }
    }
}

async function makeLuaFactory(){
    const _luaFactory = canUseLuaWorker() ? undefined : await createLuaFactory()
    async function mountFile(name:string){
        let code = ''
        for(let i = 0; i < 3; i++){
            try {
                const res = await fetch('/lua/' + name)
                if(res.status >= 200 && res.status < 300){
                    code = await res.text()
                    break
                }
            } catch (error) {}
        }
        luaJSONSource = code
        await _luaFactory?.mountFile(name,code)
    }

    await mountFile('json.lua')
    luaFactory = _luaFactory
}

async function ensureLuaFactory() {
    if (luaFactory || (canUseLuaWorker() && luaJSONSource)) return;

    const pendingFactory = (luaFactoryPromise ??= makeLuaFactory())
    try {
        await pendingFactory
    } finally {
        if (luaFactoryPromise === pendingFactory) {
            luaFactoryPromise = null
        }
    }
}

function getOrCreateEngineState(
    mode: string, 
    type: 'lua'|'py',
    ownerChaId: string,
    code: string,
    functionTimeout: number,
): ScriptingEngineState {
    const cacheKey = JSON.stringify([type, ownerChaId, mode, code, functionTimeout])
    let engineState = ScriptingEngines.get(cacheKey);
    if (engineState) {
        engineState.lastUsed = ++scriptingEngineRecency
        return engineState;
    }

    engineState = {
        mutex: new Mutex(),
        cacheKey,
        activeUses: 0,
        lastUsed: ++scriptingEngineRecency,
        type,
    };
    ScriptingEngines.set(cacheKey, engineState);
    return engineState;
}

function evictIdleScriptingEngines() {
    const maxEntries = getRuntimePerformanceBudgets().scriptingEngineCacheEntries
    while (ScriptingEngines.size > maxEntries) {
        let oldest: ScriptingEngineState | undefined
        for (const state of ScriptingEngines.values()) {
            if (state.activeUses === 0 && (!oldest || state.lastUsed < oldest.lastUsed)) {
                oldest = state
            }
        }
        if (!oldest) {
            return
        }

        ScriptingEngines.delete(oldest.cacheKey)
        if (oldest.type === 'lua') {
            oldest.engine?.global.close()
            oldest.engine = undefined
            oldest.worker?.close()
            oldest.worker = undefined
        } else {
            oldest.pyodide?.close()
            oldest.pyodide = undefined
        }
    }
}

function finishScriptingEngineUse(state: ScriptingEngineState) {
    state.activeUses--
    state.lastUsed = ++scriptingEngineRecency
    if (state.code === undefined) {
        removeScriptingEngineStateIfUnused(state)
    }
    evictIdleScriptingEngines()
}

function isLuaTimeoutError(error: unknown): boolean {
    // Async coroutine rejection preserves the Lua string; synchronous calls wrap it in Error.
    const firstLine = (typeof error === 'string' ? error : error instanceof Error ? error.message : '').split('\n', 1)[0]
    // Lua wrap/error can prepend one or more source locations to the timeout string.
    return /(?:^|:\d+: )thread timeout exceeded$/.test(firstLine)
}

function discardFailedScriptingEngineState(state: ScriptingEngineState) {
    state.code = undefined
    if (state.type === 'lua') {
        state.engine?.global.close()
        state.engine = undefined
        state.worker?.close()
        state.worker = undefined
    } else {
        state.pyodide?.close()
        state.pyodide = undefined
    }
    removeScriptingEngineStateIfUnused(state)
}

function removeScriptingEngineStateIfUnused(state: ScriptingEngineState) {
    if (state.activeUses === 0 && ScriptingEngines.get(state.cacheKey) === state) {
        ScriptingEngines.delete(state.cacheKey)
    }
}

export async function runLuaEditTrigger<T extends string | OpenAIChat[]>(
    char: character | simpleCharacterArgument,
    mode: string,
    content: T,
    meta?: object,
    conversationOperation?: ConversationOperationContext,
    onConversationCommit?: ConversationCommitObserver,
    signal?: AbortSignal,
    /** When the view asked for this display, from `nextDisplayListenerOrder`. */
    displayRequestOrder?: number,
): Promise<T> {
    if (mode !== 'editdisplay' && mode !== 'editDisplay') {
        return runLuaEditTriggerForMode(char, mode, content, meta, conversationOperation, onConversationCommit, signal)
    }
    const order = displayRequestOrder ?? nextDisplayListenerOrder()
    const run = () => runLuaEditTriggerForMode(
        char, mode, content, meta, conversationOperation, onConversationCommit, signal, order,
    )
    const endDisplayRun = beginDisplayScriptRun()
    try {
        return await run()
    } finally {
        endDisplayRun()
    }
}

async function runLuaEditTriggerForMode<T extends string | OpenAIChat[]>(
    char: character | simpleCharacterArgument,
    mode: string,
    content: T,
    meta?: object,
    conversationOperation?: ConversationOperationContext,
    onConversationCommit?: ConversationCommitObserver,
    signal?: AbortSignal,
    displayRequestOrder?: number,
): Promise<T> {
    switch (mode) {
        case 'editinput':
            mode = 'editInput'
            break
        case 'editoutput':
            mode = 'editOutput'
            break
        case 'editdisplay':
            mode = 'editDisplay'
            break
        case 'editprocess':
            return content
    }
    const displayRequest = (chat: Chat | null | undefined) => mode === 'editDisplay' && displayRequestOrder !== undefined
        ? createDisplayListenerRequest(displayRequestOrder, chat, meta)
        : undefined

    if (mode === 'editDisplay' && !conversationOperation) {
        const hasLuaTrigger = (trigger: triggerscript) =>
            trigger?.effect?.[0]?.type === 'triggerlua'
        if (
            !(char.triggerscript?.some(hasLuaTrigger)) &&
            !getModuleTriggers().some(hasLuaTrigger)
        ) {
            return content
        }
        const session = peekActiveConversationSession()
        const sourceChat = getCurrentChat()
        if (
            session?.isActive &&
            sourceChat &&
            session.materializeCompatibilityArray() === sourceChat.message
        ) {
            let mutex = luaDisplayOperationMutexes.get(session)
            if (!mutex) {
                mutex = new Mutex()
                luaDisplayOperationMutexes.set(session, mutex)
            }
            // Display listeners can write variables. Capture their operation after the
            // preceding display commits, so concurrent rows do not share a stale baseline.
            return mutex.runExclusive(async () => {
                if (
                    peekActiveConversationSession() !== session ||
                    !session.isActive ||
                    getCurrentChat() !== sourceChat ||
                    session.materializeCompatibilityArray() !== sourceChat.message
                ) {
                    return content
                }
                return runLuaEditTriggerBatch(
                    char,
                    mode,
                    content,
                    meta,
                    undefined,
                    onConversationCommit,
                    signal,
                    displayRequest(sourceChat),
                )
            })
        }
    }
    return runLuaEditTriggerBatch(
        char,
        mode,
        content,
        meta,
        conversationOperation,
        onConversationCommit,
        signal,
        displayRequest(conversationOperation ? null : getCurrentChat()),
    )
}

function createDisplayListenerRequest(order: number, chat: Chat | null | undefined, meta?: object): DisplayListenerRequest {
    const index = (meta as { index?: unknown } | undefined)?.index
    if (!chat?.id || typeof index !== 'number') return { order }
    const messageId = isMetadataOnlySelectedConversation(chat)
        ? displayedWindowMessageId(chat.id, index)
        : getHistoryMessage(chat, index)?.chatId ?? ''
    if (messageId === undefined) return { order }
    return { order, row: { conversationId: chat.id, index, messageId } }
}

/** A conversation that keeps only part of its history loaded shows its rows from the viewport source. */
function displayedWindowMessageId(conversationId: string, index: number): string | undefined {
    try {
        if (captureSelectedConversationAuthority()?.conversationId !== conversationId) return undefined
        const row = getActiveConversationViewportSource()?.snapshot().rowAt(index)
        return row ? row.message.chatId ?? '' : undefined
    } catch {
        return undefined
    }
}

async function runLuaEditTriggerBatch<T extends string | OpenAIChat[]>(
    char: character | simpleCharacterArgument,
    mode: string,
    content: T,
    meta?: object,
    conversationOperation?: ConversationOperationContext,
    onConversationCommit?: ConversationCommitObserver,
    signal?: AbortSignal,
    displayRequest?: DisplayListenerRequest,
): Promise<T> {
    let operationContext = conversationOperation
    let historyWindowCopy: HistoryWindowCopy | null = null
    let access: ScriptConversationAccess | null = null
    try {
        let data = content

        const triggers =
            char.triggerscript
                      .map<triggerscript>((v) => ({ ...v, lowLevelAccess: false }))
                      .concat(getModuleTriggers())
        if (!triggers.some((trigger) => trigger?.effect?.[0]?.type === 'triggerlua')) {
            return content
        }
        // A send that builds from a history window hands its edit listeners the window.
        const historyWindow = !operationContext && mode !== 'editDisplay'
            ? findActiveHistoryWindow(char.chaId, getCurrentChat()?.id)
            : null
        const sourceCurrent = historyWindow?.isCurrent()
        const sourceVersion = historyWindow?.mutationVersion
        const sourceMessages = historyWindow ? [...historyWindow.chat.message] : null
        const createWindowChat = historyWindow ? () => {
            if (!historyWindowCopy) {
                if (!sourceCurrent || !historyWindow.isCurrent()
                    || sourceVersion !== historyWindow.mutationVersion
                    || sourceMessages!.length !== historyWindow.chat.message.length
                    || sourceMessages!.some((message, index) => historyWindow.chat.message[index] !== message)) {
                    throw new PersistentMutationFencedError()
                }
                historyWindowCopy = openHistoryWindowCopy(historyWindow, onConversationCommit)
            }
            return historyWindowCopy.chat
        } : undefined
        if (!operationContext && !historyWindow) {
            access = createScriptConversationAccess(getCurrentChat(), getDatabase(), {
                onCommitted: onConversationCommit,
                ...(mode === 'editDisplay'
                    ? { origin: 'display' as const }
                    : { onVariablesCommitted: (keys) => requestScriptDisplayRefresh({ keys }) }),
            })
        }

        for (let trigger of triggers) {
            signal?.throwIfAborted()
            if (trigger?.effect?.[0]?.type === 'triggerlua') {
                const runResult = await runScripted(trigger.effect[0].code, {
                    char: char,
                    lowLevelAccess: false,
                    mode: mode,
                    data,
                    meta,
                    operationContext,
                    access: access ?? undefined,
                    chat: historyWindow?.chat,
                    createConversationChat: createWindowChat,
                    displayListener: displayRequest,
                    signal,
                })
                data = runResult.res ?? data
            }
        }

        return data
    } catch (error) {
        return content
    } finally {
        if (historyWindowCopy && !historyWindowCopy.commit()) {
            throw new PersistentMutationFencedError()
        }
        // A listener that changed nothing keeps its result after the conversation closes.
        access?.commitIfChanged()
    }
}

export async function runLuaInteractionTrigger(
    type: 'button' | 'form',
    char: character | simpleCharacterArgument,
    triggerKey: string,
    meta?: object,
    conversationOperation?: ConversationOperationContext,
): Promise<any> {
    const mode = type === 'button' ? 'onButtonClick' : 'onFormSubmit'
    if (conversationOperation) {
        return runLuaInteractionTriggerBatch(mode, char, triggerKey, meta, conversationOperation)
    }
    return runSerializedUserTrigger(char.chaId, getCurrentChat(), () =>
        runLuaInteractionTriggerBatch(mode, char, triggerKey, meta),
    )
}

async function runLuaInteractionTriggerBatch(
    mode: 'onButtonClick' | 'onFormSubmit',
    char: character|simpleCharacterArgument,
    triggerKey: string,
    meta?: object,
    conversationOperation?: ConversationOperationContext,
): Promise<any>{
    let runResult
    let access: ScriptConversationAccess | null = null
    try {
        const triggers = char.triggerscript.map<triggerscript>((v) => ({
            ...v,
            lowLevelAccess: char.type !== 'simple' ? char.lowLevelAccess ?? false : false
        })).concat(getModuleTriggers())
        if (!triggers.some((trigger) => trigger?.effect?.[0]?.type === 'triggerlua')) {
            return undefined
        }
        if (!conversationOperation) {
            access = createScriptConversationAccess(getCurrentChat(), getDatabase(), {
                onVariablesCommitted: (keys) => requestScriptDisplayRefresh({ keys }),
            })
        }

        for(let trigger of triggers){
            if(trigger?.effect?.[0]?.type === 'triggerlua'){
                runResult = await runScripted(trigger.effect[0].code, {
                    char: char,
                    lowLevelAccess: trigger.lowLevelAccess,
                    mode,
                    data: triggerKey,
                    meta,
                    operationContext: conversationOperation,
                    access: access ?? undefined,
                    userStarted: true,
                })
            }
        }
    } catch (error) {
        throw(error)
    } finally {
        if (access) {
            access.commit()
            if (runResult) runResult.chat = access.liveChat
        }
    }
    return runResult   
}

class PyodideContext{
    worker: Worker;
    apis: Record<string, (...args:any[]) => any> = {};
    inited: boolean = false;
    constructor(){
        this.worker = new Worker(new URL('./pyworker.ts', import.meta.url), {
            type: 'module'
        })
        this.worker.onmessage = (event:MessageEvent) => {
            if(event.data.type === 'call'){
                const { function: func, args, callId } = event.data;
                if(this.apis[func]){
                    this.apis[func](...args).then((result) => {
                        this.worker.postMessage({
                            type: 'functionResult',
                            callId: callId,
                            result: result
                        });
                    }).catch((error) => {
                        this.worker.postMessage({
                            type: 'error',
                            error: error.message,
                            id: callId
                        });
                    });
                } else {
                    this.worker.postMessage({
                        type: 'error',
                        error: `Function ${func} not found`,
                        id: callId
                    });
                }
            }
        }
    }
    declareAPI(name:string, func:(...args:any[]) => any){
        this.apis[name] = func;
    }
    async init(code:string){
        if(this.inited){
            return;
        }
        const id = crypto.randomUUID();
        return new Promise<void>((resolve, reject) => {
            this.worker.onmessage = (event:MessageEvent) => {
                if(event.data.id !== id){
                    return
                }

                if(event.data.type === 'init'){
                    this.inited = true;
                    resolve();
                } else if(event.data.type === 'error'){
                    reject(new Error(event.data.error));
                }
            };
            this.worker.postMessage({
                type: 'init',
                code: code,
                id: id,
                moduleFunctions: Object.keys(this.apis)
            });
        });
    }
    async python(call:string){
        const id = crypto.randomUUID();
        return new Promise<any>((resolve, reject) => {
            this.worker.onmessage = (event:MessageEvent) => {
                if(event.data.id !== id){
                    return
                }

                if(event.data.type === 'python'){
                    resolve(event.data.call);
                } else if(event.data.type === 'error'){
                    reject(new Error(event.data.error));
                }
            };
            this.worker.postMessage({
                type: 'python',
                call: call,
                id: id
            });
        });
    }
    close(){
        this.worker.terminate();
    }
}
