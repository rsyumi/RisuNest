import { get } from "svelte/store";
import { Mutex } from '../mutex';
import { CharEmotion, selectedCharID } from "../stores.svelte";
import { type Chat, type character, type customscript, type Database, type groupChat, type loreBook, getDatabase, getCurrentCharacter, getCurrentChat } from "../storage/database.svelte";
import { downloadFile } from "../globalApi.svelte";
import { getDeviceSettings } from "../storage/deviceSettings";
import { isStartupExcluded } from "../storage/recoveryMode.svelte";
import { alertError, alertNormal } from "../alert";
import { language } from "src/lang";
import { selectSingleFile } from "../util";
import { assetRegex, type CbsConditions, risuChatParser as risuChatParserOrg, type simpleCharacterArgument } from "../parser/parser.svelte";
import { getModuleAssets, getModuleRegexScripts, type RisuModule } from "./modules";
import { HypaProcesser } from "./memory/hypamemory";
import { runLuaEditTrigger } from "./scriptings";
import { pluginV2 } from "../plugins/plugins.svelte";
import { runTrigger } from "./triggers";
import { ByteBudgetLru } from "../util/byteBudgetLru";
import { canExecuteRegexPlanInWorker, executeRegexPlanSync, getRegexExecutionPlan, type RegexExecutionPlanEntry, type RegexExecutionResult } from "./regexExecutionPlan";
import { RegexExecutionTimeoutError, getSharedRegexWorkerClient, isRegexWorkerAvailable } from "./regexWorkerClient";
import { tryExecuteNativeRegexBatch } from "./nativeRegexBatch";
import { getRuntimePerformanceBudgets, subscribeRuntimePerformanceProfile } from "../runtimePerformanceProfile";
import { createConversationOperationContext, type ConversationOperationContext, type ConversationCommitObserver } from "./conversationOperationContext";
import { peekActiveConversationSession } from "../storage/persistentDataRuntime.svelte";
import {
    ConversationSessionInactiveError,
    ConversationSessionStaleError,
    requireCurrentConversationSession,
    type ActiveConversationPin,
    type ActiveConversationSession,
    type MessageLocator,
} from "../storage/activeConversationSession";
import {
    getChatVarFromConversation,
    setChatVarOnConversation,
} from "../parser/chatVar.svelte";

export type ScriptMode = 'editinput'|'editoutput'|'editprocess'|'editdisplay'

export interface ProcessScriptOptions {
    onConversationCommit?: ConversationCommitObserver
    cache?: 'normal' | 'bypass'
    signal?: AbortSignal
    /** true forces the Worker path, false opts out, undefined offloads whenever a Worker is available. */
    regexWorker?: boolean
    captureContext?: ProcessScriptCaptureContext
    projectedChatID?: number
    promptOperationScope?: PromptScriptOperationScope
}

export interface ProcessScriptCaptureContext {
    presetRegex: readonly customscript[]
    moduleRegexScripts: readonly customscript[]
    moduleAssets: readonly (readonly [string, string, string])[]
    dynamicAssets: boolean
    dynamicAssetsEditDisplay: boolean
    parserContext: {
        database: Database
        character: character | groupChat
        chara?: character | groupChat | string
        userName: string
        personaPrompt: string
        modules: RisuModule[]
        moduleLorebooks: loreBook[]
        selectedCharID: number
        chatVariables: Record<string, string>
        globalChatVariables: Record<string, string>
        currentTime: number
        triggerId?: string
        historyOffset?: number
    }
}

export async function processScript(
    char: character | groupChat,
    data: string,
    mode: ScriptMode,
    cbsConditions: CbsConditions = {},
    options: ProcessScriptOptions = {},
) {
    return (await processScriptFull(char, data, mode, -1, cbsConditions, options)).data
}

export function exportRegex(s?:customscript[]){
    let db = getDatabase()
    const script = s ?? db.globalscript
    const data = Buffer.from(JSON.stringify({
        type: 'regex',
        data: script
    }), 'utf-8')
    downloadFile(`regexscript_export.json`,data)
    alertNormal(language.successExport)
}

export async function importRegex(o?:customscript[]):Promise<customscript[]>{
    o = o ?? []
    const filedata = (await selectSingleFile(['json'])).data
    if(!filedata){
        return o
    }
    let db = getDatabase()
    try {
        const imported= JSON.parse(Buffer.from(filedata).toString('utf-8'))
        if(imported.type === 'regex' && imported.data){
            const datas:customscript[] = imported.data
            const script = o
            for(const data of datas){
                script.push(data)
            }
            return o
        }
        else{
            alertError("File invaid or corrupted")
        }

    } catch (error) {
        alertError(error)
    }
    return o
}

let bestMatchCache = new Map<string, string>()
let processScriptCache = createScriptCache()

function createScriptCache() {
    const budgets = getRuntimePerformanceBudgets()
    return new ByteBudgetLru<string, string>(
        budgets.scriptResultCacheBytes,
        (key, result) => 2 * (key.length + result.length),
        budgets.scriptResultCacheEntries,
    )
}

subscribeRuntimePerformanceProfile(() => {
    processScriptCache = createScriptCache()
})

function generateScriptCacheKey(
    scripts: customscript[],
    data: string,
    mode: ScriptMode,
    chatID = -1,
    cbsConditions: CbsConditions = {},
    parseCbs = (value: string) => risuChatParser(value, { chatID, cbsConditions }),
) {
    let hash = data + '|||' + mode + '|||';
    for (const script of scripts) {
        if(script.type !== mode){
            continue
        }
        hash += `${script.flag?.includes('<cbs>') ? parseCbs(script.in) : script.in}|||${script.out}${chatID}|||${script.flag ?? ''}|||${script.ableFlag ? 1 : 0}`;
    }
    return hash;
}

function cacheScript(hash:string, result:string){
    processScriptCache.set(hash, result)
}

function getScriptCache(hash:string){
    return processScriptCache.get(hash)
}

export function resetScriptCache(){
    processScriptCache = createScriptCache()
}

const HISTORY_SENSITIVE_CBS_NAMES = new Set([
    'previouscharchat', 'lastcharmessage', 'previoususerchat', 'lastusermessage',
    'lorebook', 'worldinfo', 'userhistory', 'usermessages', 'user_history',
    'charhistory', 'charmessages', 'char_history', 'authornote', 'author_note',
    'firstmsgindex', 'firstmessageindex', 'first_msg_index', 'messagetime',
    'message_time', 'messagedate', 'message_date', 'messageunixtimearray',
    'message_unixtime_array', 'messageidleduration', 'message_idle_duration',
    'idleduration', 'idle_duration', 'role', 'lastmessage', 'lastmessageid',
    'lastmessageindex', 'previouschatlog', 'previous_chat_log', 'history',
    'messages', 'pick', 'rollp', 'rollpick', 'getvar', 'addvar', 'setvar',
    'setdefaultvar',
    'personality', 'description', 'scenario', 'exampledialogue', 'examplemessage',
    'example_dialogue', 'persona', 'userpersona', 'mainprompt', 'systemprompt',
    'main_prompt', 'jb', 'jailbreak', 'globalnote', 'systemnote', 'ujb',
])

export interface ScriptConversationOwner {
    database: Database
    character: character | groupChat | null
    session: ActiveConversationSession | null
    chat: Chat | null
    version: number | null
    selectedCharacterId: string
}

function captureScriptConversationOwner(
    char: character | groupChat | simpleCharacterArgument,
    requirePromptOwnerMatch = false,
): ScriptConversationOwner {
    const db = getDatabase()
    const selectedCharacter = db.characters[get(selectedCharID)] ?? null
    if (requirePromptOwnerMatch && selectedCharacter !== char) {
        throw new ConversationSessionInactiveError()
    }
    const selectedCharacterId = selectedCharacter?.chaId ?? char.chaId
    const session = peekActiveConversationSession()
    let chat: Chat | null = null
    try {
        chat = getCurrentChat() ?? null
    } catch {
        chat = null
    }
    if (
        session?.isActive &&
        chat &&
        session.matchesConversation(selectedCharacterId, chat)
    ) {
        return {
            database: db,
            character: selectedCharacter,
            session,
            chat,
            version: session.version,
            selectedCharacterId: session.characterId,
        }
    }
    if (requirePromptOwnerMatch && session) {
        throw new ConversationSessionInactiveError()
    }
    return {
        database: db,
        character: selectedCharacter,
        session: null,
        chat,
        version: null,
        selectedCharacterId,
    }
}

function requireScriptConversationOwner(owner: ScriptConversationOwner): void {
    const db = getDatabase()
    if (db !== owner.database) throw new ConversationSessionInactiveError()
    if (
        owner.character &&
        db.characters.find((character) => character.chaId === owner.selectedCharacterId) !==
            owner.character &&
        !owner.session?.matchesConversation(owner.selectedCharacterId, getCurrentChat())
    ) {
        throw new ConversationSessionInactiveError()
    }
    if (owner.session) {
        requireCurrentConversationSession(owner.session, peekActiveConversationSession())
        if (!owner.session.canContinueGenerationFrom(owner.version!)) {
            throw new ConversationSessionStaleError(owner.version!, owner.session.version)
        }
        if (owner.chat !== getCurrentChat()) throw new ConversationSessionInactiveError()
        return
    }
    const currentCharacterId = db.characters[get(selectedCharID)]?.chaId
    if (
        currentCharacterId !== owner.selectedCharacterId ||
        (owner.chat !== null && owner.chat !== getCurrentChat())
    ) {
        throw new ConversationSessionInactiveError()
    }
}

const MUTATING_CONVERSATION_CBS_NAMES = new Set([
    'addvar', 'setvar', 'setdefaultvar',
])

function containsCbs(value: string, names: ReadonlySet<string>): boolean {
    for (const match of value.matchAll(/(?:{{|<)\s*#?\/?\s*([^}:|>\s]+)/gi)) {
        if (names.has(match[1].toLowerCase())) return true
    }
    return false
}

type ConversationAccess = 'none' | 'read-only' | 'mutating'

function classifyConversationAccess(
    plan: ReturnType<typeof getRegexExecutionPlan>,
    data: string,
): ConversationAccess {
    let access: ConversationAccess = containsCbs(data, HISTORY_SENSITIVE_CBS_NAMES)
        ? 'read-only'
        : 'none'
    if (containsCbs(data, MUTATING_CONVERSATION_CBS_NAMES)) return 'mutating'
    for (const entry of plan.entries) {
        if (
            entry.actions.includes('inject') ||
            entry.replacement.startsWith('@@inject') ||
            (entry.dynamicPattern && containsCbs(entry.pattern, MUTATING_CONVERSATION_CBS_NAMES)) ||
            containsCbs(entry.replacement, MUTATING_CONVERSATION_CBS_NAMES)
        ) {
            return 'mutating'
        }
        if (
            entry.actions.includes('repeat_back') ||
            entry.replacement.startsWith('@@repeat_back') ||
            (entry.dynamicPattern && containsCbs(entry.pattern, HISTORY_SENSITIVE_CBS_NAMES)) ||
            containsCbs(entry.replacement, HISTORY_SENSITIVE_CBS_NAMES)
        ) {
            access = 'read-only'
        }
    }
    return access
}

export class PromptScriptOperationScope {
    private operation: ConversationOperationContext | null = null
    private compatibilityPin: ActiveConversationPin | null
    private closed = false

    constructor(
        private readonly owner: ScriptConversationOwner,
        readonly usesPluginCompatibility: boolean,
    ) {
        this.compatibilityPin = usesPluginCompatibility && owner.session
            ? owner.session.acquirePin('compatibility')
            : null
    }

    get database(): Database {
        return this.owner.database
    }

    getOwner(): ScriptConversationOwner {
        this.assertOpen()
        return this.owner
    }

    parse(
        char: character | groupChat | simpleCharacterArgument,
        data: string,
        parserArgument: Parameters<typeof risuChatParserOrg>[1] = {},
    ): string {
        this.assertOwnerCurrent()
        const scripts = [
            ...(this.owner.database.presetRegex ?? []),
            ...char.customscript,
            ...getModuleRegexScripts(),
        ]
        const access = classifyConversationAccess(
            getRegexExecutionPlan(scripts, 'editprocess'),
            data,
        )
        if (access === 'mutating' && !this.usesPluginCompatibility) {
            this.ensureOperation()
        }
        const database = this.operation?.createDatabaseView(this.owner.database) ??
            this.owner.database
        const chat = this.operation?.chat ?? this.owner.chat
        return risuChatParserOrg(data, {
            ...parserArgument,
            db: database,
            selectedCharacterId: this.owner.selectedCharacterId,
            getChatVar: chat
                ? (key: string) => getChatVarFromConversation(
                    database,
                    this.owner.selectedCharacterId,
                    chat,
                    key,
                )
                : undefined,
            setChatVar: access === 'mutating' && chat
                ? (key: string, value: string) => {
                    setChatVarOnConversation(chat, key, value)
                }
                : undefined,
        })
    }

    operationFor(access: ConversationAccess): ConversationOperationContext | null {
        this.assertOwnerCurrent()
        if (access === 'mutating' && !this.usesPluginCompatibility) {
            this.ensureOperation()
        }
        return this.operation
    }

    adoptMessageId(locator: MessageLocator | undefined, messageId: string | undefined): void {
        if (!locator || !messageId || !this.operation) return
        this.operation.adoptMessageId(locator, messageId)
    }

    assertOwnerCurrent(): void {
        this.assertOpen()
        requireScriptConversationOwner(this.owner)
    }

    finish(): void {
        this.assertOwnerCurrent()
        const operation = this.operation
        this.operation = null
        this.closed = true
        try {
            if (operation) operation.commit(peekActiveConversationSession())
        } finally {
            this.releaseCompatibilityPin()
        }
    }

    finishAfterError(): void {
        if (this.closed) return
        const operation = this.operation
        this.operation = null
        this.closed = true
        try {
            if (!operation) return
            if (operation.hasPendingMutations()) {
                operation.commit(peekActiveConversationSession())
            } else {
                operation.release()
            }
        } finally {
            this.releaseCompatibilityPin()
        }
    }

    release(): void {
        if (this.closed) return
        this.closed = true
        this.operation?.release()
        this.operation = null
        this.releaseCompatibilityPin()
    }

    private ensureOperation(): void {
        if (this.operation || !this.owner.session || !this.owner.chat) return
        this.operation = createConversationOperationContext(
            this.owner.session,
            this.owner.chat,
        )
    }

    private assertOpen(): void {
        if (this.closed) throw new Error('Prompt script operation scope is closed')
    }

    private releaseCompatibilityPin(): void {
        this.compatibilityPin?.release()
        this.compatibilityPin = null
    }
}

export function createPromptScriptOperationScope(
    char: character | groupChat | simpleCharacterArgument,
    options: { pluginCompatibility?: boolean } = {},
): PromptScriptOperationScope {
    return new PromptScriptOperationScope(
        captureScriptConversationOwner(char, true),
        options.pluginCompatibility === true,
    )
}

const liveDisplayScriptMutexes = new WeakMap<ActiveConversationSession, Mutex>()

export async function processScriptFull(
    char: character | groupChat | simpleCharacterArgument,
    data: string,
    mode: ScriptMode,
    chatID = -1,
    cbsConditions: CbsConditions = {},
    options: ProcessScriptOptions = {},
) {
    options.signal?.throwIfAborted()
    if (
        mode !== 'editdisplay' ||
        options.captureContext ||
        options.promptOperationScope
    ) {
        return processScriptFullImpl(
            char,
            data,
            mode,
            chatID,
            cbsConditions,
            options,
        )
    }
    const owner = captureScriptConversationOwner(char)
    const session = owner.session
    if (!session || !owner.chat) {
        return processScriptFullImpl(
            char,
            data,
            mode,
            chatID,
            cbsConditions,
            options,
        )
    }
    let mutex = liveDisplayScriptMutexes.get(session)
    if (!mutex) {
        mutex = new Mutex()
        liveDisplayScriptMutexes.set(session, mutex)
    }
    let invalidated = false
    const unsubscribe = session.subscribe((event) => {
        if (
            !event ||
            (!event.displayVariableUpdate &&
                !session.canContinueGenerationFrom(event.previousVersion))
        ) {
            invalidated = true
        }
    })
    try {
        // Protect the whole live script pipeline. A Lua-only lock still lets the
        // next row change variables while this row awaits plugins or regex work.
        return await mutex.runExclusive(async () => {
            options.signal?.throwIfAborted()
            requireCurrentConversationSession(
                session,
                peekActiveConversationSession(),
            )
            if (invalidated) {
                throw new ConversationSessionStaleError(
                    owner.version!,
                    session.version,
                )
            }
            // Earlier display-owned variable commits are expected while waiting.
            // Navigation and external edits still invalidate the captured owner.
            requireScriptConversationOwner({
                ...owner,
                version: session.version,
            })
            return processScriptFullImpl(
                char,
                data,
                mode,
                chatID,
                cbsConditions,
                options,
            )
        })
    } finally {
        unsubscribe()
    }
}

async function processScriptFullImpl(char:character|groupChat|simpleCharacterArgument, data:string, mode:ScriptMode, chatID = -1, cbsConditions:CbsConditions = {}, options:ProcessScriptOptions = {}){
    options.signal?.throwIfAborted()
    const captureContext = options.captureContext
    const promptOperationScope = captureContext ? undefined : options.promptOperationScope
    let db = captureContext?.parserContext.database ?? promptOperationScope?.database ?? getDatabase()
    let emoChanged = false
    if (!captureContext) {
        data = await runLuaEditTrigger(
            char,
            mode,
            data,
            { index: chatID },
            undefined,
            options.onConversationCommit,
        )
        options.signal?.throwIfAborted()
    }

    if(mode === 'editdisplay' && !captureContext){
        const currentChar = getCurrentCharacter()
        if(currentChar.type !== 'group'){
            try{
                const perf = performance.now()
                const d = await runTrigger(currentChar, 'display', {
                    chat: getCurrentChat(),
                    displayMode: true,
                    displayData: data
                })
    
                data = d?.displayData ?? data
                console.log('Trigger time', performance.now() - perf)
            }
            catch(e){
                console.error(e)
            }
        }
    }
    options.signal?.throwIfAborted()

    const conversationOwner = captureContext
        ? null
        : promptOperationScope?.getOwner() ?? captureScriptConversationOwner(char)
    const usesPluginCompatibility = !captureContext &&
        conversationOwner !== null && pluginV2[mode].size > 0
    const compatibilityPin = usesPluginCompatibility &&
        !promptOperationScope?.usesPluginCompatibility && conversationOwner.session
        ? conversationOwner.session.acquirePin('compatibility')
        : null
    if(usesPluginCompatibility){
        try {
            for(const plugin of pluginV2[mode]){
                const res = await plugin(data)
                options.signal?.throwIfAborted()
                if(res !== null && res !== undefined){
                    data = res
                }
            }
        } catch (error) {
            compatibilityPin?.release()
            throw error
        }
    }

    let scripts: customscript[]
    let plan: ReturnType<typeof getRegexExecutionPlan>
    let conversationOperation: ConversationOperationContext | null = null
    let conversationAccess: ConversationAccess = 'none'
    let readPin: ActiveConversationPin | null = null
    try {
        const globalRegexOff = isStartupExcluded(
            'regex',
            getDeviceSettings().startupExclusions,
        )
        scripts = globalRegexOff
            ? [...char.customscript]
            : [
                  ...(captureContext?.presetRegex ?? db.presetRegex ?? []),
                  ...char.customscript,
                  ...(captureContext?.moduleRegexScripts ?? getModuleRegexScripts()),
              ]
        plan = getRegexExecutionPlan(scripts, mode)
        conversationAccess = captureContext
            ? 'none'
            : classifyConversationAccess(plan, data)
        const needsConversationOperation = conversationAccess === 'mutating'
        if (conversationAccess !== 'none' && conversationOwner) {
            requireScriptConversationOwner(conversationOwner)
        }
        conversationOperation = promptOperationScope?.operationFor(conversationAccess) ?? null
        readPin = conversationAccess === 'read-only' && !conversationOperation &&
            !compatibilityPin && !promptOperationScope?.usesPluginCompatibility &&
            conversationOwner?.session
            ? conversationOwner.session.acquirePin('compatibility')
            : null
        if (
            !promptOperationScope && !conversationOperation && needsConversationOperation &&
            conversationOwner?.session && conversationOwner.chat
        ) {
            conversationOperation = createConversationOperationContext(
                conversationOwner.session,
                conversationOwner.chat,
                options.onConversationCommit,
            )
        }
    } catch (error) {
        conversationOperation?.release()
        readPin?.release()
        compatibilityPin?.release()
        throw error
    }
    const needsConversationOperation = conversationAccess === 'mutating'
    const ownsConversationOperation = conversationOperation !== null &&
        promptOperationScope === undefined
    const operationDatabase = conversationOperation?.createDatabaseView(db) ?? db
    const operationChat = conversationOperation?.chat ?? conversationOwner?.chat ?? null
    const parseCbs = (value: string) => captureContext
        ? risuChatParserOrg(value, {
            chatID,
            projectedChatID: options.projectedChatID,
            historyOffset: captureContext.parserContext.historyOffset,
            cbsConditions,
            db: captureContext.parserContext.database,
            chara: captureContext.parserContext.chara ?? captureContext.parserContext.character,
            userName: captureContext.parserContext.userName,
            personaPrompt: captureContext.parserContext.personaPrompt,
            modules: captureContext.parserContext.modules,
            moduleLorebooks: captureContext.parserContext.moduleLorebooks,
            selectedCharID: captureContext.parserContext.selectedCharID,
            selectedCharacterId: captureContext.parserContext.character.chaId,
            chatVariables: captureContext.parserContext.chatVariables,
            globalChatVariables: captureContext.parserContext.globalChatVariables,
            currentTime: captureContext.parserContext.currentTime,
            triggerId: captureContext.parserContext.triggerId,
            role: cbsConditions.chatRole,
        })
        : risuChatParserOrg(value, {
            chatID,
            cbsConditions,
            db: operationDatabase,
            selectedCharacterId: conversationOwner?.selectedCharacterId,
            getChatVar: conversationAccess !== 'none' && operationChat && conversationOwner
                ? (key: string) => getChatVarFromConversation(
                    operationDatabase,
                    conversationOwner.selectedCharacterId,
                    operationChat,
                    key,
                )
                : undefined,
            setChatVar: needsConversationOperation && operationChat
                ? (key: string, value: string) => {
                    setChatVarOnConversation(operationChat, key, value)
                }
                : undefined,
        })
    let conversationOperationCommitted = false
    const finish = <T>(result: T): T => {
        options.signal?.throwIfAborted()
        if (conversationOperation && ownsConversationOperation) {
            conversationOperation.commit(peekActiveConversationSession(), {
                origin: mode === 'editdisplay' ? 'display' : undefined,
            })
            conversationOperationCommitted = true
        }
        else if (conversationAccess === 'read-only' && conversationOwner) {
            requireScriptConversationOwner(conversationOwner)
        }
        return result
    }

    try {
    data = parseCbs(data)
    const useResultCache = options.cache !== 'bypass'
    const hash = useResultCache
        ? generateScriptCacheKey(scripts, data, mode, chatID, cbsConditions, parseCbs)
        : undefined
    if(!useResultCache){
        for(const script of scripts){
            if(script.type === mode && script.flag?.includes('<cbs>')){
                parseCbs(script.in)
            }
        }
    }
    if(hash !== undefined){
        const cached = getScriptCache(hash)
        if(cached !== undefined){
            return finish({data: cached, emoChanged: false})
        }
    }
    
    if(scripts.length === 0){
        if(hash !== undefined){
            cacheScript(hash, data)
        }
        return finish({data, emoChanged})
    }

    const parse = parseCbs

    function executeScript(entry:RegexExecutionPlanEntry){
        const script = entry.script
        
        if(script.in === ''){
            return
        }

        const outScript = entry.replacement
        const flag = entry.flags
        let reg: RegExp
        if(entry.dynamicPattern){
            reg = new RegExp(parse(entry.pattern), flag)
        }
        else{
            if(entry.compileError !== undefined){
                throw entry.compileError
            }
            if(entry.compiledRegex === undefined){
                throw new Error('Regex execution plan entry was not compiled')
            }
            reg = entry.compiledRegex
        }
        reg.lastIndex = 0

            if(outScript.startsWith('@@') || entry.actions.length > 0){
                if(reg.test(data)){
                    if(outScript.startsWith('@@emo ')){
                        const emoName = script.out.substring(6).trim()
                        let charemotions = get(CharEmotion)
                        let tempEmotion = charemotions[char.chaId]
                        if(!tempEmotion){
                            tempEmotion = []
                        }
                        if(tempEmotion.length > 4){
                            tempEmotion.splice(0, 1)
                        }
                        if(char.type !== 'simple'){
                            for(const emo of char.emotionImages){
                                if(emo[0] === emoName){
                                    const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                                    tempEmotion.push(emos)
                                    charemotions[char.chaId] = tempEmotion
                                    CharEmotion.set(charemotions)
                                    emoChanged = true
                                    break
                                }
                            }
                        }
                    }
                    else if((outScript.startsWith('@@inject') || entry.actions.includes('inject')) && chatID !== -1){
                        if (!captureContext) {
                            const selchar = operationDatabase.characters.find(
                                (candidate) => candidate.chaId === conversationOwner?.selectedCharacterId,
                            )
                            if (!selchar) throw new ConversationSessionInactiveError()
                            selchar.chats[selchar.chatPage].message[chatID].data = data
                        }
                        reg.lastIndex = 0
                        data = data.replace(reg, "")
                    }
                    else if(
                        outScript.startsWith('@@move_top') || outScript.startsWith('@@move_bottom') ||
                        entry.actions.includes('move_top') || entry.actions.includes('move_bottom')
                    ){
                        const isGlobal = flag.includes('g')
                        reg.lastIndex = 0
                        const matchAll = isGlobal ? data.matchAll(reg) : [data.match(reg)]
                        reg.lastIndex = 0
                        data = data.replace(reg, "")
                        for(const matched of matchAll){
                            if(matched){
                                const inData = matched[0]
                                let out = outScript.replace('@@move_top ', '').replace('@@move_bottom ', '')
                                    .replace(/(?<!\$)\$[0-9]+/g, (v)=>{
                                        const index = parseInt(v.substring(1))
                                        if(index < matched.length){
                                            return matched[index]
                                        }
                                        return v
                                    })
                                    .replace(/\$\&/g, inData)
                                    .replace(/(?<!\$)\$<([^>]+)>/g, (v) => {
                                        const groupName = parseInt(v.substring(2, v.length - 1))
                                        if(matched.groups && matched.groups[groupName]){
                                            return matched.groups[groupName]
                                        }
                                        return v
                                    })
                                if(outScript.startsWith('@@move_top') || entry.actions.includes('move_top')){
                                    data = out + '\n' +data
                                }
                                else{
                                    data = data + '\n' + out
                                }
                            }
                        }
                    }
                    else{
                        reg.lastIndex = 0
                        data = parse(data.replace(reg, outScript))
                    }
                }
                else{
                    if((outScript.startsWith('@@repeat_back') || entry.actions.includes('repeat_back'))  && chatID !== -1){
                        const v = outScript.split(' ', 2)[1]
                        const selchar = captureContext
                            ? operationDatabase.characters[captureContext.parserContext.selectedCharID]
                            : operationDatabase.characters.find(
                                (candidate) => candidate.chaId === conversationOwner?.selectedCharacterId,
                            )
                        if (!selchar) throw new ConversationSessionInactiveError()
                        const chat = selchar.chats[selchar.chatPage]
                        let lastChat = chat.fmIndex === -1 ? selchar.firstMessage : selchar.alternateGreetings[chat.fmIndex]
                        const historyChatID = options.projectedChatID ?? chatID
                        let pointer = historyChatID - 1
                        while(pointer >= 0){
                            if(chat.message[pointer].role === chat.message[historyChatID].role){
                                lastChat = chat.message[pointer].data
                                break
                            }
                            pointer--
                        }

                        reg.lastIndex = 0
                        const r = lastChat.match(reg)
                        if(!v){
                            data = data + r[0]
                        }
                        else if(r[0]){
                            switch(v){
                                case 'end':
                                    data = data + r[0]
                                    break
                                case 'start':
                                    data = r[0] + data
                                    break
                                case 'end_nl':
                                    data = data + "\n" + r[0]
                                    break
                                case 'start_nl':
                                    data = r[0] + "\n" + data
                                    break
                            }

                        }                        
                    }
                }
            }
            else{
                data = parse(data.replace(reg, outScript))
            }
    }

    if(plan.requiresHostExecution){
        for (const entry of plan.entries){
            try {
                executeScript(entry)
            } catch (error) {
                console.error(error)
            }
        }
    }
    else if((options.regexWorker ?? isRegexWorkerAvailable()) && mode === 'editoutput' && canExecuteRegexPlanInWorker(plan, data)){
        let result: RegexExecutionResult | undefined
        try {
            result = await tryExecuteNativeRegexBatch(plan, data, { signal: options.signal })
            options.signal?.throwIfAborted()
        } catch (error) {
            if(options.signal?.aborted){
                throw error
            }
            console.error(error)
        }
        if(result === undefined){
            try {
                result = await getSharedRegexWorkerClient().execute(plan, data, { signal: options.signal })
                options.signal?.throwIfAborted()
            } catch (error) {
                // A pathological ruleset must not be retried on the UI thread, and a cancelled
                // generation must stay cancelled. Anything else means the Worker is unusable here.
                if(error instanceof RegexExecutionTimeoutError || options.signal?.aborted){
                    throw error
                }
                console.error(error)
                result = executeRegexPlanSync(plan, data, parse)
            }
        }
        data = result.data
        for(const error of result.errors){
            console.error(error.error)
        }
    }
    else{
        data = executeRegexPlanSync(plan, data, parse).data
    }

    

    const dynamicAssets = captureContext?.dynamicAssets ?? db.dynamicAssets
    const dynamicAssetsEditDisplay = captureContext?.dynamicAssetsEditDisplay ?? db.dynamicAssetsEditDisplay
    if(dynamicAssets && (char.type === 'simple' || char.type === 'character') && char.additionalAssets && char.additionalAssets.length > 0){
        if((!dynamicAssetsEditDisplay && mode === 'editdisplay')
            || mode === 'editinput' || mode === 'editprocess'){
            if(hash !== undefined){
                cacheScript(hash, data)
            }
            return finish({data, emoChanged})
        }
        const assetNames = char.additionalAssets.map((v) => v[0])

        const moduleAssets = captureContext?.moduleAssets ?? getModuleAssets()
        if(moduleAssets.length > 0){
            for(const asset of moduleAssets){
                assetNames.push(asset[0])
            }
        }

        const processer = new HypaProcesser()
        await processer.addText(assetNames)
        options.signal?.throwIfAborted()
        const matches = data.matchAll(assetRegex)

        for(const match of matches){
            const type = match[1]
            const assetName = match[2]
            const cacheKey = char.chaId + '::' + assetName
            if(type !== 'emotion' && type !== 'source'){
                if(bestMatchCache.has(cacheKey)){
                    data = data.replaceAll(match[0], `{{${type}::${bestMatchCache.get(cacheKey)}}}`)
                }
                else if(!assetNames.includes(assetName)){
                    const searched = await processer.similaritySearch(assetName)
                    options.signal?.throwIfAborted()
                    const bestMatch = searched[0]
                    if(bestMatch){
                        data = data.replaceAll(match[0], `{{${type}::${bestMatch}}}`)
                        bestMatchCache.set(cacheKey, bestMatch)
                    }
                }
            }
        }
    }

    if(hash !== undefined){
        cacheScript(hash, data)
    }

    return finish({data, emoChanged})
    } catch (error) {
        try {
            if (
                !options.signal?.aborted &&
                ownsConversationOperation &&
                conversationOperation?.hasPendingMutations()
            ) {
                conversationOperation.commit(peekActiveConversationSession(), {
                    origin: mode === 'editdisplay' ? 'display' : undefined,
                })
                conversationOperationCommitted = true
            } else if (conversationAccess === 'read-only' && conversationOwner) {
                requireScriptConversationOwner(conversationOwner)
            }
        } catch (cleanupError) {
            console.error(cleanupError)
        }
        throw error
    } finally {
        if (ownsConversationOperation && conversationOperation && !conversationOperationCommitted) {
            conversationOperation.release()
        }
        readPin?.release()
        compatibilityPin?.release()
    }
}


const rgx = /(?:{{|<)(.+?)(?:}}|>)/gm
export const risuChatParser = risuChatParserOrg
