import { get, writable } from "svelte/store";
import { type character, type groupChat, type MessageGenerationInfo, type Chat, type MessagePresetInfo, changeToPreset, setCurrentChat, type Message } from "../storage/database.svelte";
import { DBState } from '../stores.svelte';
import { CharEmotion, selectedCharID } from "../stores.svelte";
import { ChatTokenizer, tokenize, tokenizeNum } from "../tokenizer";
import { language } from "../../lang";
import { alertError, alertToast } from "../alert";
import { parseChatML } from "../parser/chatML";
import { loadLoreBookV3Prompt } from "./lorebook.svelte";
import { findCharacterbyId, getAuthorNoteDefaultText, getPersonaPrompt, getUserName, isLastCharPunctuation, trimUntilPunctuation, parseToggleSyntax, prebuiltAssetCommand } from "../util";
import { requestChatData } from "./request/request";
import { stableDiff } from "./stableDiff";
import { createPromptScriptOperationScope, processScript, processScriptFull, risuChatParser, type PromptScriptOperationScope } from "./scripts";
import { exampleMessage } from "./exampleMessages";
import { sayTTS } from "./tts";
import { supaMemory } from "./memory/supaMemory";
import { v4 } from "uuid";
import { groupOrder } from "./group";
import { runTrigger } from "./triggers";
import { HypaProcesser } from "./memory/hypamemory";
import { additionalInformations } from "./embedding/addinfo";
import { getInlayAsset } from "./files/inlays";
import { getGenerationModelString } from "./models/modelString";
import { connectionOpen, peerRevertChat, peerSafeCheck, peerSync } from "../sync/multiuser";
import { runInlayScreen } from "./inlayScreen";
import { addRerolls } from "./prereroll";
import { runImageEmbedding } from "./transformers";
import { hanuraiMemory } from "./memory/hanuraiMemory";
import { hypaMemoryV2 } from "./memory/hypav2";
import { runLuaEditTrigger } from "./scriptings";
import { getModelInfo, LLMFlags } from "../model/modellist";
import { hypaMemoryV3 } from "./memory/hypav3";
import { getModuleAssets, getModuleToggles } from "./modules";
import { readImage } from "../globalApi.svelte";
import {
    chatOutputListenerProvenance,
    pluginCompatibility,
    pluginV2,
} from "../plugins/plugins.svelte";
import { dispatchChatOutputListeners } from '../plugins/pluginChatOutputListeners'
import { createProductionPluginChatOutputProjector } from '../plugins/pluginDatabaseAccess'
import { activatePresetChainForRequest } from "./presetChain";
import {
    reserveGeneration,
    type GenerationReservation,
} from './generationState'
import {
    acknowledgeGenerationCompletion,
    acquireCompleteConversation,
    captureSelectedConversationTarget,
    getActiveConversationSession,
    invalidateActiveConversationSession,
} from '../storage/persistentDataRuntime.svelte'
import { ensureCurrentConversationMessageIds } from '../conversationMutations'
import { requireCurrentConversationSession } from '../storage/activeConversationSession'
import {
    beginPinnedConversationHistoryOperation,
    createCompatibilityConversationHistorySnapshot,
    type ConversationHistoryOperation,
} from '../storage/conversationHistoryOperation'
import {
    adoptTriggeredChat,
    createLivePromptHistoryCompatibilitySnapshot,
    ensurePromptHistoryEntryId,
    ensurePromptHistoryMessageId,
    iteratePromptHistory,
    selectPromptHistory,
    type PromptHistoryCompatibilitySnapshot,
} from './promptHistory'
import { runCurrentChatParserPass } from './currentChatParserPass'
import { applyGenerationErrorResponse } from './generationErrorResponse'
import { applyGenerationResponse } from './generationResponseApplication'
import {
    SelectedConversationPromotionStaleError,
    type CompleteConversationLease,
} from '../storage/activeWorkingSet.svelte'
import { beginAndroidGenerationKeepAlive, endAndroidGenerationKeepAlive } from '../androidGenerationKeepAlive'
import { beginIOSGeneration, notifyIOSGenerationComplete } from "../iosNative";
import { isTauriIOS } from "../platform";

export { doingChat } from './generationState'

export interface OpenAIChat{
    role: 'system'|'user'|'assistant'|'function'
    content: string
    memo?:string
    name?:string
    removable?:boolean
    attr?:string[]
    multimodals?: MultiModal[]
    thoughts?: string[]
    cachePoint?: boolean
}

const projectPluginChatOutput = createProductionPluginChatOutputProjector(
    <T>(value: T) => $state.snapshot(value) as T,
)

export async function runChatOutputListeners(char: any, chat: any, characterIndex: number, chatIndex: number, messageIndex: number, signal?: AbortSignal){
    if(pluginV2.chatOutput.size === 0){
        return
    }
    await dispatchChatOutputListeners({
        listeners: pluginV2.chatOutput,
        provenance: chatOutputListenerProvenance,
        profile: pluginCompatibility.profile,
        char,
        chat,
        characterIndex,
        chatIndex,
        messageIndex,
        signal,
        snapshot: <T>(value: T) => $state.snapshot(value) as T,
        projectScalable: projectPluginChatOutput,
        onError: (error) => console.error(error),
    })
}

export interface MultiModal{
    type:'image'|'video'|'audio'|'signature'
    base64:string,
    height?:number,
    width?:number
}

export interface requestTokenPart{
    name:string
    tokens:number
}

export const chatProcessStage = writable(0)
export const abortChat = writable(false)
export let requestTokenParts:{[key:string]:requestTokenPart[]} = {}
export let previewFormated:OpenAIChat[] = []
export let previewBody:string = ''

function beginPromptHistoryOperation(
    owner: character | groupChat,
    chat: Chat,
): ConversationHistoryOperation {
    const session = getActiveConversationSession()
    if (session?.matchesConversation(owner.chaId, chat)) {
        return beginPinnedConversationHistoryOperation(session)
    }
    return createCompatibilityConversationHistorySnapshot({
        characterId: owner.chaId,
        conversationId: chat.id ?? 'compatibility-current-conversation',
        messages: chat.message,
        storeRevision: session?.storeRevision ?? 0,
    })
}

function hasMismatchedActiveConversationSession(): boolean {
    const activeSession = getActiveConversationSession()
    if (!activeSession) return false

    const selectedCharacter = DBState.db.characters[get(selectedCharID)]
    const selectedConversation = selectedCharacter?.chats[selectedCharacter.chatPage]
    return !selectedCharacter
        || !selectedConversation
        || !activeSession.matchesConversation(selectedCharacter.chaId, selectedConversation)
}

interface GenerationCompletionLifecycle {
    responseCompleted: boolean
    reroll: boolean
    responseApplied: boolean
    acknowledgementAttempted: boolean
}

export async function notifyGenerationCompletion(result: string): Promise<void> {
    if (DBState.db.notification) {
        try {
            if (isTauriIOS) {
              await notifyIOSGenerationComplete();
            } else if ((await Notification.requestPermission()) === "granted") {
              const notification = new Notification("RisuNest", {
                body: result,
              });
              notification.onclick = () => window.focus();
            }
        } catch {}
    }
    void peerSync()
}

export async function sendChat(chatProcessIndex = -1,arg:{
    chatAdditonalTokens?:number,
    signal?:AbortSignal,
    continue?:boolean,
    usedContinueTokens?:number,
    preview?:boolean
    previewPrompt?:boolean
} = {}, inheritedReservation?: GenerationReservation):Promise<boolean> {
    const ownedReservation = inheritedReservation ? null : reserveGeneration()
    const reservation = inheritedReservation ?? ownedReservation
    if (!reservation?.isCurrent()) return false
    let completeLease: CompleteConversationLease | null = null
    const lifecycle: GenerationCompletionLifecycle = {
        responseCompleted: false,
        reroll: !!DBState.db.characters[get(selectedCharID)]?.chats[
            DBState.db.characters[get(selectedCharID)]?.chatPage
        ]?.rerollRecovery,
        responseApplied: false,
        acknowledgementAttempted: false,
    }
    let enteredGeneration = false
    let generationReturned = false
    let generationKeepAliveAcquired = false
    let iosGeneration:
      | Awaited<ReturnType<typeof beginIOSGeneration>>
      | undefined;
    try {
        const target = captureSelectedConversationTarget()
        if (target) {
            try {
                completeLease = await acquireCompleteConversation('generation', target)
            } catch (error) {
                if (error instanceof SelectedConversationPromotionStaleError) return false
                throw error
            }
        }
        enteredGeneration = true
        generationKeepAliveAcquired = beginAndroidGenerationKeepAlive()
        iosGeneration = await beginIOSGeneration(arg.signal);
        const result = await sendChatInternal(
          chatProcessIndex,
          { ...arg, signal: iosGeneration.signal },
          lifecycle,
          reservation,
        );
        generationReturned = true
        return result
    } catch (error) {
        if (lifecycle.reroll && lifecycle.responseCompleted && !arg.signal?.aborted) {
            alertError(error)
            return true
        }
        throw error
    } finally {
        if (!lifecycle.reroll && lifecycle.responseApplied && !lifecycle.acknowledgementAttempted) {
            lifecycle.acknowledgementAttempted = true
            try {
                await acknowledgeGenerationCompletion()
            } catch (acknowledgeError) {
                console.error(acknowledgeError)
            }
        }
        completeLease?.release()
        endAndroidGenerationKeepAlive(generationKeepAliveAcquired)
        await iosGeneration
          ?.dispose()
          .catch((error) =>
            console.error("iOS generation cleanup failed", error),
          );
        ownedReservation?.release({
            preserveBusy: enteredGeneration && !generationReturned,
        })
    }
}

async function sendChatInternal(chatProcessIndex: number,arg:{
    chatAdditonalTokens?:number,
    signal?:AbortSignal,
    continue?:boolean,
    usedContinueTokens?:number,
    preview?:boolean
    previewPrompt?:boolean
}, lifecycle: GenerationCompletionLifecycle, reservation: GenerationReservation):Promise<boolean> {

    chatProcessStage.set(0)
    const abortSignal = arg.signal ?? (new AbortController()).signal
    
    // NOTE: `throwError()` can be called before these are populated (e.g. HypaV3 early validation errors).
    // Keep them declared up-front to avoid TDZ ReferenceErrors in production builds.
    let selectedChar = -1
    let selectedChat = -1
    let currentChar:character
    let generationInfo:MessageGenerationInfo|undefined = undefined

    const stageTimings = {
        stage1Start: 0,
        stage2Start: 0,
        stage3Start: 0,
        stage4Start: 0,
        stage1Duration: 0,
        stage2Duration: 0,
        stage3Duration: 0,
        stage4Duration: 0
    }

    let isAborted = false
    let findCharCache:{[key:string]:character} = {}
    function findCharacterbyIdwithCache(id:string){
        const d = findCharCache[id]
        if(!!d){
            return d
        }
        else{
            const r = findCharacterbyId(id)
            findCharCache[id] = r
            return r
        }
    }


    function runCurrentChatFunction(chat:Chat){
        return runCurrentChatParserPass({
            chat,
            database: DBState.db,
            ownerCharacterId: nowChatroom.chaId,
            parserCharacter: currentChar,
            session: getActiveConversationSession(),
            parser: risuChatParser,
        })
    }

    function reformatContent(data:string){
        if(chatProcessIndex === -1){
            return data.trim()
        }
        return data.trim()
    }

    function throwError(error:string){
        if(!DBState?.db?.inlayErrorResponse){
            alertError(error)
            return
        }

        try{
            const db = DBState.db

            // Prefer already-resolved selection, but fall back to current store/db pointers.
            const sc = selectedChar >= 0 ? selectedChar : get(selectedCharID)
            const charRoom = db.characters?.[sc]
            if(!charRoom){
                alertError(error)
                return
            }
            const st = selectedChat >= 0 ? selectedChat : charRoom.chatPage
            const chatRoom = charRoom.chats?.[st]
            if(!chatRoom || !Array.isArray(chatRoom.message)){
                alertError(error)
                return
            }

            const suffix = `\n\`\`\`risuerror\n${error}\n\`\`\``
            const m:Message = {
                role: 'char',
                data: `\`\`\`risuerror\n${error}\n\`\`\``,
                time: Date.now(),
            }
            if(currentChar?.chaId){
                m.saying = currentChar.chaId
            }
            if(generationInfo){
                m.generationInfo = generationInfo
            }
            const session = getActiveConversationSession()
            const applied = applyGenerationErrorResponse({
                session,
                getCurrentSession: getActiveConversationSession,
                characterId: charRoom.chaId,
                chat: chatRoom,
                getCurrentChat: () => {
                    const currentOwner = DBState.db.characters?.[sc]
                    return currentOwner === charRoom && currentOwner.chatPage === st
                        ? currentOwner.chats?.[st]
                        : null
                },
                isOwnerCurrent: () => get(selectedCharID) === sc,
                suffix,
                appendMessage: m,
            })
            if (!applied) alertError(error)
            return
        }
        catch(e){
            console.error(e)
            alertError(error)
            return
        }
    }

    const generationSetupSession = getActiveConversationSession()
    if (hasMismatchedActiveConversationSession()) return false

    if (chatProcessIndex === -1) {
        await activatePresetChainForRequest(
            DBState.db,
            changeToPreset,
            Math.random,
            (name) => alertToast(`Cannot find preset: ${name}`),
        )
    }

    if(connectionOpen){
        chatProcessStage.set(4)
        const peerSafe = await peerSafeCheck()
        if(!peerSafe){
            peerRevertChat()
            throwError(language.otherUserRequesting)
            return false
        }
        await peerSync()
        chatProcessStage.set(0)
    }

    if (
        (generationSetupSession && getActiveConversationSession() !== generationSetupSession)
        || hasMismatchedActiveConversationSession()
    ) return false

    DBState.db.statics.messages += 1
    selectedChar = get(selectedCharID)
    const nowChatroom = DBState.db.characters[selectedChar]
    nowChatroom.lastInteraction = Date.now()
    selectedChat = nowChatroom.chatPage
    const selectedConversation = nowChatroom.chats[selectedChat]
    const activeSession = getActiveConversationSession()
    if (
        activeSession &&
        !activeSession.matchesConversation(nowChatroom.chaId, selectedConversation)
    ) return false
    ensureCurrentConversationMessageIds(
        nowChatroom,
        selectedConversation,
        activeSession,
        v4,
    )
    
    let promptInfo: MessagePresetInfo = {}
    let initialPresetNameForPromptInfo = null
    let initialPromptTogglesForPromptInfo: {
        key: string,
        value: string,
    }[] = []
    if(DBState.db.promptInfoInsideChat){
        initialPresetNameForPromptInfo = DBState.db.botPresets[DBState.db.botPresetsId]?.name ?? ''
        initialPromptTogglesForPromptInfo = parseToggleSyntax(DBState.db.customPromptTemplateToggle + getModuleToggles())
            .flatMap(toggle => {
                const raw = DBState.db.globalChatVariables[`toggle_${toggle.key}`]
                if (toggle.type === 'select' || toggle.type === 'text') {
                    return [{ key: toggle.value, value: toggle.options[raw] }];
                }
                if (raw === '1') {
                    return [{ key: toggle.value, value: 'ON' }];
                }
                return [];
            })

        promptInfo = {
            promptName: initialPresetNameForPromptInfo,
            promptToggles: initialPromptTogglesForPromptInfo,
        }
    }

    let caculatedChatTokens = 0
    if(DBState.db.aiModel.startsWith('gpt')){
        caculatedChatTokens += 5
    }
    else{
        caculatedChatTokens += 3
    }

    if(nowChatroom.type === 'group'){
        if(chatProcessIndex === -1){
            const charNames =nowChatroom.characters.map((v) => findCharacterbyIdwithCache(v).name)

            const messages = nowChatroom.chats[nowChatroom.chatPage].message
            const lastMessage = messages[messages.length-1]
            let order = nowChatroom.characters.map((v,i) => {
                return {
                    id: v,
                    talkness: nowChatroom.characterActive[i] ? nowChatroom.characterTalks[i] : -1,
                    index: i
                }
            }).filter((v) => {
                return v.talkness > 0
            })
            if(!nowChatroom.orderByOrder){
                order = groupOrder(order, lastMessage?.data).filter((v) => {
                    if(v.id === lastMessage?.saying){
                        return false
                    }
                    return true
                })
            }
            for(let i=0;i<order.length;i++){
                const r = await sendChat(order[i].index, {
                    chatAdditonalTokens: caculatedChatTokens,
                    signal: abortSignal
                }, reservation)
                if(!r){
                    return false
                }
            }
            return true
        }
        else{
            currentChar = findCharacterbyIdwithCache(nowChatroom.characters[chatProcessIndex])
            if(!currentChar){
                throwError(`cannot find character: ${nowChatroom.characters[chatProcessIndex]}`)
                return false
            }
        }
    }
    else{
        currentChar = nowChatroom
    }

    let chatAdditonalTokens = arg.chatAdditonalTokens ?? caculatedChatTokens
    const tokenizer = new ChatTokenizer(chatAdditonalTokens, DBState.db.aiModel.startsWith('gpt') ? 'noName' : 'name')
    let currentChat = runCurrentChatFunction(nowChatroom.chats[selectedChat])
    nowChatroom.chats[selectedChat] = currentChat
    let maxContextTokens = DBState.db.maxContext

    chatProcessStage.set(1)
    stageTimings.stage1Start = Date.now()
    let unformated = {
        'main':([] as OpenAIChat[]),
        'jailbreak':([] as OpenAIChat[]),
        'chats':([] as OpenAIChat[]),
        'lorebook':([] as OpenAIChat[]),
        'globalNote':([] as OpenAIChat[]),
        'authorNote':([] as OpenAIChat[]),
        'lastChat':([] as OpenAIChat[]),
        'description':([] as OpenAIChat[]),
        'postEverything':([] as OpenAIChat[]),
        'personaPrompt':([] as OpenAIChat[])
    }

    let promptTemplate = safeStructuredClone(DBState.db.promptTemplate)
    const usingPromptTemplate = !!promptTemplate
    if(promptTemplate){
        let hasPostEverything = false
        for(const card of promptTemplate){
            if(card.type === 'postEverything'){
                hasPostEverything = true
                break
            }
        }

        if(!hasPostEverything){
            promptTemplate.push({
                type: 'postEverything'
            })
        }
    }
    if(currentChar.utilityBot && (!(usingPromptTemplate && DBState.db.promptSettings.utilOverride))){
        promptTemplate = [
            {
              "type": "plain",
              "text": "",
              "role": "system",
              "type2": "main"
            },
            {
              "type": "description",
            },
            {
              "type": "lorebook",
            },
            {
              "type": "chat",
              "rangeStart": 0,
              "rangeEnd": "end"
            },
            {
              "type": "plain",
              "text": "",
              "role": "system",
              "type2": "globalNote"
            },
            {
                'type': "postEverything"
            }
        ]
    }

    if((!currentChar.utilityBot) && (!promptTemplate)){
        const mainp = currentChar.systemPrompt?.replaceAll('{{original}}', DBState.db.mainPrompt) || DBState.db.mainPrompt


        function formatPrompt(data:string){
            if(!data.startsWith('@@')){
                data = "@@system\n" + data
            }
            const parts = data.split(/@@@?(user|assistant|system)\n/);
  
            // Initialize empty array for the chat objects
            const chatObjects: OpenAIChat[] = [];
            
            // Loop through the parts array two elements at a time
            for (let i = 1; i < parts.length; i += 2) {
              const role = parts[i] as 'user' | 'assistant' | 'system';
              const content = parts[i + 1]?.trim() || '';
              chatObjects.push({ role, content });
            }

            return chatObjects;
        }

        unformated.main.push(...formatPrompt(risuChatParser(mainp + ((DBState.db.additionalPrompt === '' || (!DBState.db.promptPreprocess)) ? '' : `\n${DBState.db.additionalPrompt}`), {chara: currentChar})))
    
        if(DBState.db.jailbreakToggle){
            unformated.jailbreak.push(...formatPrompt(risuChatParser(DBState.db.jailbreak, {chara: currentChar})))
        }
    
        unformated.globalNote.push(...formatPrompt(risuChatParser(currentChar.replaceGlobalNote?.replaceAll('{{original}}', DBState.db.globalNote) || DBState.db.globalNote, {chara:currentChar})))
    }

    let baseDescriptionPrompt:OpenAIChat|null = null
    let beforeDescriptionPrompts:OpenAIChat[] = []
    let afterDescriptionPrompts:OpenAIChat[] = []

    if(currentChat.note){
        unformated.authorNote.push({
            role: 'system',
            content: risuChatParser(currentChat.note, {chara: currentChar})
        })
    }
    else if(getAuthorNoteDefaultText() !== ''){
        unformated.authorNote.push({
            role: 'system',
            content: risuChatParser(getAuthorNoteDefaultText(), {chara: currentChar})
        })
    }

    if(DBState.db.chainOfThought && (!(usingPromptTemplate && DBState.db.promptSettings.customChainOfThought))){
        unformated.postEverything.push({
            role: 'system',
            content: `<instruction> - before respond everything, Think step by step as a ai assistant how would you respond inside <Thoughts> xml tag. this must be less than 5 paragraphs.</instruction>`
        })
    }

    {
        let description = risuChatParser((DBState.db.promptPreprocess ? DBState.db.descriptionPrefix: '') + currentChar.desc, {chara: currentChar})

        const additionalInfo = await additionalInformations(currentChar, currentChat)

        if(additionalInfo){
            description += '\n\n' + risuChatParser(additionalInfo, {chara:currentChar})
        }

        if(currentChar.personality){
            description += risuChatParser("\n\nDescription of {{char}}: " + currentChar.personality, {chara: currentChar})
        }

        if(currentChar.scenario){
            description += risuChatParser("\n\nCircumstances and context of the dialogue: " + currentChar.scenario, {chara: currentChar})
        }

        baseDescriptionPrompt = {
            role: 'system',
            content: description
        }
        unformated.description.push(baseDescriptionPrompt)

        if(nowChatroom.type === 'group'){
            const systemMsg = `[Write the next reply only as ${currentChar.name}]`
            unformated.postEverything.push({
                role: 'system',
                content: systemMsg
            })
        }
    }

    const loreHistory = beginPromptHistoryOperation(nowChatroom, currentChat)
    const lorepmt = await (async () => {
        try {
            return await loadLoreBookV3Prompt(loreHistory)
        } finally {
            loreHistory.dispose()
        }
    })()

    const positionRegex = /{{position::(.+?)}}/g
    const replaceposition = (text:string):{text:string, replaced:boolean} => {
        let replaced = false
        const result = text.replace(positionRegex, (match, p1) => {
            replaced = true
            const posMatch = 'pt_' + p1
            const matchingPrompts: string[] = []
            for (const v of lorepmt.actives) {
                if (v.pos === posMatch) {
                    matchingPrompts.push(v.prompt)
                }
            }
            return matchingPrompts.join('\n')
        })
        return {text: result, replaced}
    }

    // maxDepth controls how many levels of nesting are resolved. Currently set to 5, adjust if needed.
    const resolvePosition = (text:string, maxDepth:number = 5) => {
        let result = text
        for(let i=0; i<maxDepth;i++) {
            const r = replaceposition(result)
            result = r.text
            if(!r.replaced) break
        }
        result = result.replace(positionRegex, '')
        return result
    }

    const normalActives = lorepmt.actives.filter(v => {
        return v.pos === '' && v.inject === null
    })
    console.log(normalActives)

    for(const lorebook of normalActives){
        unformated.lorebook.push({
            role: lorebook.role,
            content: risuChatParser(resolvePosition(lorebook.prompt), {chara: currentChar})
        })
    }

    const descActives = lorepmt.actives.filter(v => {
        return v.pos === 'after_desc' || v.pos === 'before_desc' || v.pos === 'personality' || v.pos === 'scenario'
    })

    for(const lorebook of descActives){
        const c = {
            role: lorebook.role,
            content: risuChatParser(resolvePosition(lorebook.prompt), {chara: currentChar})
        }
        if(lorebook.pos === 'before_desc'){
            beforeDescriptionPrompts.unshift(c)
            unformated.description.unshift(c)
        }
        else{
            afterDescriptionPrompts.push(c)
            unformated.description.push(c)
        }
    }

    if(DBState.db.personaPrompt){
        unformated.personaPrompt.push({
            role: 'system',
            content: risuChatParser(getPersonaPrompt(), {chara: currentChar})
        })
    }
    
    if(currentChar.inlayViewScreen){
        if(currentChar.viewScreen === 'emotion'){
            unformated.postEverything.push({
                role: 'system',
                content: currentChar.newGenData.emotionInstructions.replaceAll('{{slot}}', currentChar.emotionImages.map((v) => v[0]).join(', '))
            })
        }
        if(currentChar.viewScreen === 'imggen'){
            unformated.postEverything.push({
                role: 'system',
                content: currentChar.newGenData.instructions
            })
        }
    }

    const postEverythingLorebooks = lorepmt.actives.filter(v => {
        return v.pos === 'depth' && v.depth === 0 && v.role !== 'assistant'
    })
    for(const lorebook of postEverythingLorebooks){
        unformated.postEverything.push({
            role: lorebook.role,
            content: risuChatParser(resolvePosition(lorebook.prompt), {chara: currentChar})
        })
    }

    //Since assistant needs to be prefill, we need to add assistant lorebooks after user/system lorebooks
    const postEverythingAssistantLorebooks = lorepmt.actives.filter(v => {
        return v.pos === 'depth' && v.depth === 0 && v.role === 'assistant'
    })

    const injectionLorebooks = lorepmt.actives.filter(v => {
        return v.inject && !v.inject.lore
    })

    const injectionLorePosSet = new Set<string>()
    for(const lorebook of injectionLorebooks){
        injectionLorePosSet.add(lorebook.inject.location)
    }
    
    for(const lorebook of postEverythingAssistantLorebooks){
        unformated.postEverything.push({
            role: lorebook.role,
            content: risuChatParser(resolvePosition(lorebook.prompt), {chara: currentChar})
        })
    }

    //await tokenize currernt
    let currentTokens = DBState.db.maxResponse
    let supaMemoryCardUsed = false
    
    //for unexpected error
    currentTokens += 50
    
    const positionParser = (text:string, loc:string) => {
        console.log(injectionLorePosSet)
        if(injectionLorePosSet.has(loc)){
            const matchings = injectionLorebooks.filter(v => {
                return v.inject.location === loc
            })
            for(const lore of matchings){
                switch(lore.inject.operation){
                    case 'append':{
                        text += ' ' + lore.prompt
                        break
                    }
                    case 'prepend':{
                        text = lore.prompt + ' ' + text
                        break
                    }
                    case 'replace':{
                        text = text.replace(lore.inject.param, lore.prompt)
                        break
                    }
                }
            }
        }

        return resolvePosition(text)
    }

    let hasCachePoint = false
    const convertPromptRole = {
        "system": "system",
        "user": "user",
        "bot": "assistant",
    } as const

    function applyPromptBlockRole(chats:OpenAIChat[], role?: 'user'|'bot'|'system'){
        console.log("Applying ", chats, role)
        if(!role){
            return
        }
        for(const chat of chats){
            chat.role = convertPromptRole[role]
        }
    }

    function getDescriptionPrompts(role?: 'user'|'bot'|'system'){
        const pmt = [
            ...safeStructuredClone(beforeDescriptionPrompts),
            ...(baseDescriptionPrompt ? [safeStructuredClone(baseDescriptionPrompt)] : []),
            ...safeStructuredClone(afterDescriptionPrompts)
        ]
        if(baseDescriptionPrompt){
            applyPromptBlockRole([pmt[beforeDescriptionPrompts.length]], role)
        }
        return pmt
    }

    if(promptTemplate){
        const template = promptTemplate

        async function tokenizeChatArray(chats:OpenAIChat[]){
            currentTokens += await tokenizer.tokenizeChats(chats)
        }

        for(const card of template){
            switch(card.type){
                case 'persona':{
                    let pmt = safeStructuredClone(unformated.personaPrompt)
                    applyPromptBlockRole(pmt, card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(positionParser(card.innerFormat,card.type), {chara: currentChar}).replace('{{slot}}', pmt[i].content)
                        }
                    }

                    await tokenizeChatArray(pmt)
                    break
                }
                case 'description':{
                    let pmt = getDescriptionPrompts(card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(positionParser(card.innerFormat,card.type), {chara: currentChar}).replace('{{slot}}', pmt[i].content)
                        }
                    }

                    await tokenizeChatArray(pmt)
                    break
                }
                case 'authornote':{
                    let pmt = safeStructuredClone(unformated.authorNote)
                    applyPromptBlockRole(pmt, card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(positionParser(card.innerFormat,card.type), {chara: currentChar}).replace('{{slot}}', pmt[i].content || card.defaultText || '')
                        }
                    }

                    await tokenizeChatArray(pmt)
                    break
                }
                case 'lorebook':{
                    await tokenizeChatArray(unformated.lorebook)
                    break
                }
                case 'postEverything':{
                    await tokenizeChatArray(unformated.postEverything)
                    if(usingPromptTemplate && DBState.db.promptSettings.postEndInnerFormat){
                        await tokenizeChatArray([{
                            role: 'system',
                            content: DBState.db.promptSettings.postEndInnerFormat
                        }])
                    }
                    break
                }
                case 'plain':
                case 'jailbreak':
                case 'cot':{
                    if((!DBState.db.jailbreakToggle) && (card.type === 'jailbreak')){
                        continue
                    }
                    if((!DBState.db.chainOfThought) && (card.type === 'cot')){
                        continue
                    }

                    const posType = card.type === 'plain' ? card.type2 : card.type
                    let content = positionParser(card.text, posType)

                    if(card.type2 === 'globalNote'){
                        if(currentChar.replaceGlobalNote){
                            content = positionParser(currentChar.replaceGlobalNote, posType).replaceAll('{{original}}', content)
                        }
                        
                        if(currentChar.prebuiltAssetCommand && !card.text.includes('{{//@customimageinstruction}}')){
                            content += prebuiltAssetCommand
                        }
                        content = (risuChatParser(content, {chara: currentChar, role: card.role}))
                    }
                    else if(card.type2 === 'main'){
                        content = (risuChatParser(content, {chara: currentChar, role: card.role}))
                    }
                    else{
                        content = risuChatParser(content, {chara: currentChar, role: card.role})
                    }

                    const prompt:OpenAIChat ={
                        role: convertPromptRole[card.role],
                        content: content
                    }

                    await tokenizeChatArray([prompt])
                    break
                }
                case 'chatML':{
                    let prompts = parseChatML(card.text)
                    await tokenizeChatArray(prompts)
                    break
                }
                case 'chat':{
                    let start = card.rangeStart
                    let end = (card.rangeEnd === 'end') ? unformated.chats.length : card.rangeEnd
                    if(start === -1000){
                        start = 0
                        end = unformated.chats.length
                    }
                    if(start < 0){
                        start = unformated.chats.length + start
                        if(start < 0){
                            start = 0
                        }
                    }
                    if(end < 0){
                        end = unformated.chats.length + end
                        if(end < 0){
                            end = 0
                        }
                    }
                    
                    if(start >= end){
                        break
                    }
                    let chats = unformated.chats.slice(start, end)

                    if(usingPromptTemplate && DBState.db.promptSettings.sendChatAsSystem && (!card.chatAsOriginalOnSystem)){
                        chats = systemizeChat(chats)
                    }
                    await tokenizeChatArray(chats)
                    break
                }
                case 'memory':{
                    supaMemoryCardUsed = true
                    break
                }
                case 'cache':{
                    hasCachePoint = true
                    break
                }
            }
        }
    }
    else{
        for(const key in unformated){
            const chats = unformated[key] as OpenAIChat[]
            currentTokens += await tokenizer.tokenizeChats(chats)
        }
    }
    
    const examples = exampleMessage(currentChar, getUserName())

    currentTokens += await tokenizer.tokenizeChats(examples)

    let chats:OpenAIChat[] = examples

    if(!DBState.db.aiModel.startsWith('novelai') && !DBState.db?.promptSettings?.trimStartNewChat){
        chats.push({
            role: 'system',
            content: '[Start a new chat]',
            memo: "NewChat"
        })
    }

    
    const greetingHistory = beginPromptHistoryOperation(nowChatroom, currentChat)
    const greetingHistorySelection = (() => {
        try {
            return selectPromptHistory(greetingHistory)
        } finally {
            greetingHistory.dispose()
        }
    })()

    if(nowChatroom.type !== 'group' && !greetingHistorySelection.resetByAllBefore){
        const firstMsg = currentChat.fmIndex === -1 ? nowChatroom.firstMessage : nowChatroom.alternateGreetings[currentChat.fmIndex]

        const chat:OpenAIChat = {
            role: 'assistant',
            content: await (processScript(nowChatroom,
                risuChatParser(firstMsg, {chara: currentChar}),
            'editprocess'))
        }

        if(usingPromptTemplate && DBState.db.promptSettings.sendName){
            chat.content = `${currentChar.name}: ${chat.content}`
            chat.attr = ['nameAdded']
        }
        chats.push(chat)
        currentTokens += await tokenizer.tokenizeChat(chat)
    }
    
    const triggerSession = getActiveConversationSession()
    const triggerSessionSnapshot = triggerSession?.matchesConversation(nowChatroom.chaId, currentChat) === true
        ? {
            session: triggerSession,
            version: triggerSession.version,
            messages: currentChat.message,
        }
        : null
    const triggerResult = await runTrigger(currentChar, 'start', {chat: currentChat})
    if(triggerResult){
        if (triggerSessionSnapshot && triggerResult.chat !== currentChat) {
            requireCurrentConversationSession(
                triggerSessionSnapshot.session,
                getActiveConversationSession(),
            )
            currentChat = adoptTriggeredChat(
                triggerSessionSnapshot.session,
                triggerSessionSnapshot.version,
                triggerSessionSnapshot.messages,
                triggerResult.chat,
            )
        } else {
            currentChat = triggerResult.chat
        }
        setCurrentChat(currentChat)
        currentTokens += triggerResult.tokens
        if(triggerResult.stopSending){
            return false
        }
    }

    const requiresLivePromptCompatibility = (pluginV2.editprocess?.size ?? 0) > 0
    const promptHistory = beginPromptHistoryOperation(nowChatroom, currentChat)
    let promptHistoryCompatibilitySnapshot: PromptHistoryCompatibilitySnapshot | null = null
    let promptScriptOperationScope: PromptScriptOperationScope | null = null
    try {
    promptScriptOperationScope = createPromptScriptOperationScope(nowChatroom, {
        pluginCompatibility: requiresLivePromptCompatibility,
    })
    const promptHistorySelection = selectPromptHistory(promptHistory)
    const promptHistoryEntries = requiresLivePromptCompatibility
        ? (promptHistoryCompatibilitySnapshot = createLivePromptHistoryCompatibilitySnapshot(
            currentChat.message,
            promptHistorySelection,
        )).entries
        : iteratePromptHistory(promptHistory, promptHistorySelection)
    for(const entry of promptHistoryEntries){
        promptScriptOperationScope.assertOwnerCurrent()
        const { relativeIndex: index } = entry
        let msg: Message
        if (requiresLivePromptCompatibility) {
            ensurePromptHistoryMessageId(entry.message, v4)
            msg = entry.message
        } else {
            msg = ensurePromptHistoryEntryId(promptHistory, entry, v4)
        }
        promptScriptOperationScope?.adoptMessageId(entry.locator, msg.chatId)
        const parsedMessage = promptScriptOperationScope
            ? promptScriptOperationScope.parse(nowChatroom, msg.data, {
                chara: currentChar,
                role: msg.role,
            })
            : risuChatParser(msg.data, {chara: currentChar, role: msg.role})
        let formatedChat = (await processScriptFull(nowChatroom, parsedMessage, 'editprocess', index, {
            chatRole: msg.role,
        }, {
            promptOperationScope: promptScriptOperationScope ?? undefined,
        })).data
        let name = ''
        if(msg.role === 'char'){
            if(msg.saying){
                name = `${findCharacterbyIdwithCache(msg.saying).name}`
            }
            else{
                name = `${currentChar.name}`
            }
        }
        else if(msg.role === 'user'){
            name = `${getUserName()}`
        }
        let inlays:string[] = []
        if(msg.role === 'char'){
            formatedChat = formatedChat.replace(/{{(inlay|inlayed|inlayeddata)::(.+?)}}/g, (
                match: string,
                p1: string,
                p2: string
            ) => {
                if(p2 && p1 === 'inlayeddata'){
                    inlays.push(p2)
                }
                return ''
            })
        }
        else{
            const inlayMatch = formatedChat.match(/{{(inlay|inlayed|inlayeddata)::(.+?)}}/g)
            if(inlayMatch){
                for(const inlay of inlayMatch){
                    inlays.push(inlay)
                }
            }
        }

        let multimodal:MultiModal[] = []
        const modelinfo = getModelInfo(DBState.db.aiModel)
        if(inlays.length > 0){
            for(const inlay of inlays){
                const inlayName = inlay.replace('{{inlayed::', '').replace('{{inlay::', '').replace('}}', '').replace('{{inlayeddata::', '')
                const inlayData = await getInlayAsset(inlayName)
                if(inlayData?.type === 'image'){
                    if(modelinfo.flags.includes(LLMFlags.hasImageInput)){
                        multimodal.push({
                            type: 'image',
                            base64: inlayData.data,
                            width: inlayData.width,
                            height: inlayData.height
                        })
                    }
                    else{
                        const captionResult = await runImageEmbedding(inlayData.data) 
                        formatedChat += `[${captionResult[0].generated_text}]`
                    }
                }
                if(inlayData?.type === 'video' || inlayData?.type === 'audio'){
                    if(multimodal.length === 0){
                        multimodal.push({
                            type: inlayData.type,
                            base64: inlayData.data
                        })
                    }
                }
                if(inlayData?.type === 'signature'){
                    multimodal.push({
                        type: 'signature',
                        base64: inlayData.data
                    })
                }
                formatedChat = formatedChat.replace(inlay, '')
            }
        }

        let attr:string[] = []
        let role:'user'|'assistant'|'system' = msg.role === 'user' ? 'user' : 'assistant'

        if(
            (nowChatroom.type === 'group' && findCharacterbyIdwithCache(msg.saying).chaId !== currentChar.chaId) ||
            (nowChatroom.type === 'group' && DBState.db.groupOtherBotRole === 'assistant') ||
            (usingPromptTemplate && DBState.db.promptSettings.sendName)
        ){
            const form = DBState.db.groupTemplate || `<{{char}}\'s Message>\n{{slot}}\n</{{char}}\'s Message>`
            const groupTemplate = promptScriptOperationScope
                ? promptScriptOperationScope.parse(nowChatroom, form, {
                    chara: findCharacterbyIdwithCache(msg.saying).name,
                })
                : risuChatParser(form, {chara: findCharacterbyIdwithCache(msg.saying).name})
            formatedChat = groupTemplate.replace('{{slot}}', formatedChat)
            switch(DBState.db.groupOtherBotRole){
                case 'user':
                case 'assistant':
                case 'system':
                    role = DBState.db.groupOtherBotRole
                    break
                default:
                    role = 'assistant'
                    break
            }
        }
        let thoughts:string[] = []
        const maxThoughtDepth = DBState.db.promptSettings?.maxThoughtTagDepth ?? -1
        formatedChat = formatedChat.replace(/<Thoughts>(.+)<\/Thoughts>/gms, (match, p1) => {
            if(maxThoughtDepth === -1 || (maxThoughtDepth - promptHistorySelection.messageCount) <= index){
                thoughts.push(p1)
            }
            return ''
        })

        const assetPromises:Promise<void>[] = []
        formatedChat = formatedChat.replace(/\{\{asset_?prompt::(.+?)\}\}/gmsiu, (match, p1) => {
            const moduleAssets = getModuleAssets()
            const assets = (currentChar.additionalAssets ?? []).concat(moduleAssets)
            const asset = assets.find(v => {
                return v[0] === p1
            })
            if(asset){
                assetPromises.push((async () => {
                    const assetDataBuf = await readImage(asset[1])
                    multimodal.push({
                        type: "image",
                        base64: `data:image/png;base64,${Buffer.from(assetDataBuf).toString('base64')}`
                    })
                })())
            }
            else if(p1 === 'icon'){
                assetPromises.push((async () => {
                    const assetDataBuf = await readImage(currentChar.image ?? '')
                    multimodal.push({
                        type: "image",
                        base64: `data:image/png;base64,${Buffer.from(assetDataBuf).toString('base64')}`
                    })
                })())
            }
            return ''          
        })
        await Promise.all(assetPromises)

        const chat:OpenAIChat = {
            role: role,
            content: formatedChat,
            memo: msg.chatId,
            attr: attr,
            multimodals: multimodal,
            thoughts: thoughts
        }
        if(chat.multimodals.length === 0){
            delete chat.multimodals
        }
        chats.push(chat)
        currentTokens += await tokenizer.tokenizeChat(chat)
    }
    promptScriptOperationScope?.finish()
    promptScriptOperationScope = null
    } catch (error) {
        promptScriptOperationScope?.finishAfterError()
        promptScriptOperationScope = null
        throw error
    } finally {
        promptScriptOperationScope?.release()
        promptHistoryCompatibilitySnapshot?.dispose()
        promptHistory.dispose()
    }
    console.log(JSON.stringify(chats, null, 2))

    const depthPrompts = lorepmt.actives.filter(v => {
        return (v.pos === 'depth' && v.depth > 0) || v.pos === 'reverse_depth'
    })

    for(const depthPrompt of depthPrompts){
        const chat:OpenAIChat = {
            role: depthPrompt.role,
            content: risuChatParser(resolvePosition(depthPrompt.prompt), {chara: currentChar})
        }
        currentTokens += await tokenizer.tokenizeChat(chat)
    }
    
    if(nowChatroom.supaMemory && (DBState.db.supaModelType !== 'none' || DBState.db.hanuraiEnable || DBState.db.hypav2 || DBState.db.hypaV3)){
        stageTimings.stage1Duration = Date.now() - stageTimings.stage1Start
        chatProcessStage.set(2)
        stageTimings.stage2Start = Date.now()
        if(DBState.db.hanuraiEnable){
            const hn = await hanuraiMemory(chats, {
                currentTokens,
                maxContextTokens,
                tokenizer
            })

            if(hn === false){
                return false
            }

            chats = hn.chats
            currentTokens = hn.tokens
        }
        else if(DBState.db.hypav2){
            console.log("Current chat's hypaV2 Data: ", currentChat.hypaV2Data)
            const sp = await hypaMemoryV2(chats, currentTokens, maxContextTokens, currentChat, nowChatroom, tokenizer)
            if(sp.error){
                console.log(sp)
                throwError(sp.error)
                return false
            }
            chats = sp.chats
            currentTokens = sp.currentTokens
            currentChat.hypaV2Data = sp.memory ?? currentChat.hypaV2Data
            DBState.db.characters[selectedChar].chats[selectedChat].hypaV2Data = currentChat.hypaV2Data

            currentChat = DBState.db.characters[selectedChar].chats[selectedChat];
            console.log("[Expected to be updated] chat's HypaV2Data: ", currentChat.hypaV2Data)
        }
        else if(DBState.db.hypaV3){
            console.log("Current chat's hypaV3 Data: ", currentChat.hypaV3Data)
            const sp = await hypaMemoryV3(chats, currentTokens, maxContextTokens, currentChat, nowChatroom, tokenizer)
            if(sp.error){
                // Save new summary
                if (sp.memory) {
                    currentChat.hypaV3Data = sp.memory
                    DBState.db.characters[selectedChar].chats[selectedChat].hypaV3Data = currentChat.hypaV3Data
                }
                console.log(sp)
                throwError(sp.error)
                return false
            }
            chats = sp.chats
            currentTokens = sp.currentTokens
            currentChat.hypaV3Data = sp.memory ?? currentChat.hypaV3Data
            DBState.db.characters[selectedChar].chats[selectedChat].hypaV3Data = currentChat.hypaV3Data
    
            currentChat = DBState.db.characters[selectedChar].chats[selectedChat];
            console.log("[Expected to be updated] chat's HypaV3Data: ", currentChat.hypaV3Data)
        }
        else{
            const sp = await supaMemory(chats, currentTokens, maxContextTokens, currentChat, nowChatroom, tokenizer, {
                asHyper: DBState.db.hypaMemory
            })
            if(sp.error){
                throwError(sp.error)
                return false
            }
            chats = sp.chats
            currentTokens = sp.currentTokens
            currentChat.supaMemoryData = sp.memory ?? currentChat.supaMemoryData
            DBState.db.characters[selectedChar].chats[selectedChat].supaMemoryData = currentChat.supaMemoryData
            console.log(currentChat.supaMemoryData)
            currentChat.lastMemory = sp.lastId ?? currentChat.lastMemory;
        }
        stageTimings.stage2Duration = Date.now() - stageTimings.stage2Start
        chatProcessStage.set(1)
    }
    else{
        stageTimings.stage1Duration = Date.now() - stageTimings.stage1Start
        while(currentTokens > maxContextTokens){
            if(chats.length <= 1){
                throwError(language.errors.toomuchtoken + "\n\nRequired Tokens: " + currentTokens)

                return false
            }

            currentTokens -= await tokenizer.tokenizeChat(chats[0])
            chats.splice(0, 1)
        }
        currentChat.lastMemory = chats[0].memo
    }

    let biases:[string,number][] = DBState.db.bias.concat(currentChar.bias).map((v) => {
        return [risuChatParser(v[0].replaceAll("\\n","\n").replaceAll("\\r","\r").replaceAll("\\\\","\\"), {chara: currentChar}),v[1]]
    })

    let memories:OpenAIChat[] = []



    if(!promptTemplate){
        unformated.lastChat.push(chats[chats.length - 1])
        chats.splice(chats.length - 1, 1)
    }

    unformated.chats = chats.map((v) => {
        if(v.memo !== 'supaMemory' && v.memo !== 'hypaMemory'){
            v.removable = true
        }
        else if(supaMemoryCardUsed){
            memories.push(v)
            return {
                role: 'system',
                content: '',
            } as OpenAIChat
        }
        else{
            v.content = `<Previous Conversation>${v.content}</Previous Conversation>`
        }
        return v
    }).filter((v) => {
        return v.content.trim() !== '' || (v.multimodals && v.multimodals.length > 0)
    })

    for(const depthPrompt of depthPrompts){
        const chat:OpenAIChat = {
            role: depthPrompt.role,
            content: risuChatParser(resolvePosition(depthPrompt.prompt), {chara: currentChar})
        }
        const depth = depthPrompt.pos === 'depth' ? (depthPrompt.depth) : (unformated.chats.length - depthPrompt.depth)
        unformated.chats.splice(depth,0,chat)
    }

    if(triggerResult){
        if(triggerResult.additonalSysPrompt.promptend){
            unformated.postEverything.push({
                role: 'system',
                content: triggerResult.additonalSysPrompt.promptend
            })
        }
        if(triggerResult.additonalSysPrompt.historyend){
            unformated.lastChat.push({
                role: 'system',
                content: triggerResult.additonalSysPrompt.historyend
            })
        }
        if(triggerResult.additonalSysPrompt.start){
            unformated.lastChat.unshift({
                role: 'system',
                content: triggerResult.additonalSysPrompt.start
            })
        }
    }

    
    //make into one

    let formated:OpenAIChat[] = []
    const formatOrder = safeStructuredClone(DBState.db.formatingOrder)
    if(formatOrder){
        formatOrder.push('postEverything')
    }

    //continue chat model
    if(arg.continue && (DBState.db.aiModel.startsWith('claude') || DBState.db.aiModel.startsWith('gpt') || DBState.db.aiModel.startsWith('openrouter') || DBState.db.aiModel.startsWith('reverse_proxy'))){
        unformated.postEverything.push({
            role: 'system',
            content: '[Continue the last response]'
        })
    }

    function pushPrompts(cha:OpenAIChat[]){
        for(const chat of cha){
            if(!chat.content.trim() && !(chat.multimodals && chat.multimodals.length > 0)){
                continue
            }
            if(!(DBState.db.aiModel.startsWith('gpt') || DBState.db.aiModel.startsWith('claude') || DBState.db.aiModel === 'openrouter' || DBState.db.aiModel === 'reverse_proxy')){
                formated.push(chat)
                continue
            }
            if(chat.role === 'system'){
                const endf = formated.at(-1)
                if(endf && endf.role === 'system' && endf.memo === chat.memo && endf.name === chat.name){
                    formated[formated.length - 1].content += '\n\n' + chat.content
                }
                else{
                    formated.push(chat)
                }
                formated.at(-1).content += ''
            }
            else{
                formated.push(chat)
            }
        }
    }

    let promptBodyformatedForChatStore: OpenAIChat[] = []
    function pushPromptInfoBody(role: "function" | "system" | "user" | "assistant", fmt: string, promptBody: OpenAIChat[]) {
        if(!fmt.trim()){
            return
        }
        promptBody.push({
            role: role,
            content: risuChatParser(fmt),
        })
    }

    if(promptTemplate){
        const template = promptTemplate

        for(const card of template){
            switch(card.type){
                case 'persona':{
                    let pmt = safeStructuredClone(unformated.personaPrompt)
                    applyPromptBlockRole(pmt, card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(positionParser(card.innerFormat,card.type), {chara: currentChar}).replace('{{slot}}', pmt[i].content)

                            if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat){
                                pushPromptInfoBody(pmt[i].role, card.innerFormat, promptBodyformatedForChatStore)
                            }
                        }
                    }

                    pushPrompts(pmt)
                    break
                }
                case 'description':{
                    let pmt = getDescriptionPrompts(card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(positionParser(card.innerFormat,card.type), {chara: currentChar}).replace('{{slot}}', pmt[i].content)
                            
                            if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat){
                                pushPromptInfoBody(pmt[i].role, card.innerFormat, promptBodyformatedForChatStore)
                            }
                        }
                    }

                    pushPrompts(pmt)
                    break
                }
                case 'authornote':{
                    let pmt = safeStructuredClone(unformated.authorNote)
                    applyPromptBlockRole(pmt, card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(positionParser(card.innerFormat,card.type), {chara: currentChar}).replace('{{slot}}', pmt[i].content || card.defaultText || '')
                            
                            if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat){
                                pushPromptInfoBody(pmt[i].role, card.innerFormat, promptBodyformatedForChatStore)
                            }
                        }
                    }

                    pushPrompts(pmt)
                    break
                }
                case 'lorebook':{
                    pushPrompts(unformated.lorebook)
                    break
                }
                case 'postEverything':{
                    pushPrompts(unformated.postEverything)
                    if(usingPromptTemplate && DBState.db.promptSettings.postEndInnerFormat){
                        pushPrompts([{
                            role: 'system',
                            content: DBState.db.promptSettings.postEndInnerFormat
                        }])
                    }
                    break
                }
                case 'plain':
                case 'jailbreak':
                case 'cot':{
                    if((!DBState.db.jailbreakToggle) && (card.type === 'jailbreak')){
                        continue
                    }
                    if((!DBState.db.chainOfThought) && (card.type === 'cot')){
                        continue
                    }

                    const posType = card.type === 'plain' ? card.type2 : card.type
                    let content = positionParser(card.text, posType)

                    if(card.type2 === 'globalNote'){
                        if(currentChar.replaceGlobalNote){
                            content = positionParser(currentChar.replaceGlobalNote, posType).replaceAll('{{original}}', content)
                        }
                        if(currentChar.prebuiltAssetCommand && !card.text.includes('{{//@customimageinstruction}}')){
                            content += prebuiltAssetCommand
                        }
                        content = (risuChatParser(content, {chara: currentChar, role: card.role}))
                    }
                    else if(card.type2 === 'main'){
                        content = (risuChatParser(content, {chara: currentChar, role: card.role}))
                    }
                    else{
                        content = risuChatParser(content, {chara: currentChar, role: card.role})
                    }

                    const prompt:OpenAIChat ={
                        role: convertPromptRole[card.role],
                        content: content
                    }

                    if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat && card.type2 !== 'globalNote'){
                        pushPromptInfoBody(prompt.role, prompt.content, promptBodyformatedForChatStore)
                    }

                    pushPrompts([prompt])
                    break
                }
                case 'chatML':{
                    let prompts = parseChatML(card.text)
                    pushPrompts(prompts)
                    break
                }
                case 'chat':{
                    let start = card.rangeStart
                    let end = (card.rangeEnd === 'end') ? unformated.chats.length : card.rangeEnd
                    if(start === -1000){
                        start = 0
                        end = unformated.chats.length
                    }
                    if(start < 0){
                        start = unformated.chats.length + start
                        if(start < 0){
                            start = 0
                        }
                    }
                    if(end < 0){
                        end = unformated.chats.length + end
                        if(end < 0){
                            end = 0
                        }
                    }
                    
                    if(start >= end){
                        break
                    }

                    let chats = unformated.chats.slice(start, end)
                    if(usingPromptTemplate && DBState.db.promptSettings.sendChatAsSystem && (!card.chatAsOriginalOnSystem)){
                        chats = systemizeChat(chats)
                    }
                    pushPrompts(chats)

                    if(DBState.db.automaticCachePoint && !hasCachePoint){
                        let pointer = formated.length - 1
                        let depthRemaining = 3
                        while(pointer >= 0){
                            if(depthRemaining === 0){
                                break
                            }
                            if(formated[pointer].role === 'user'){
                                formated[pointer].cachePoint = true
                                depthRemaining--
                            }
                            pointer--
                        }
                    }
                    break
                }
                case 'memory':{
                    let pmt = safeStructuredClone(memories)
                    applyPromptBlockRole(pmt, card.role2)
                    if(card.innerFormat && pmt.length > 0){
                        for(let i=0;i<pmt.length;i++){
                            pmt[i].content = risuChatParser(card.innerFormat, {chara: currentChar}).replace('{{slot}}', pmt[i].content)

                            if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat){
                                pushPromptInfoBody(pmt[i].role, card.innerFormat, promptBodyformatedForChatStore)
                            }
                        }
                    }

                    pushPrompts(pmt)
                    break
                }
                case 'cache':{
                    let pointer = formated.length - 1
                    let depthRemaining = card.depth
                    while(pointer >= 0){
                        if(depthRemaining === 0){
                            break
                        }
                        if(formated[pointer].role === card.role || card.role === 'all'){
                            formated[pointer].cachePoint = true
                            depthRemaining--
                        }
                        pointer--
                    }
                    break
                }
            }
        }
    }
    else{
        for(let i=0;i<formatOrder.length;i++){
            const cha = unformated[formatOrder[i]]
            pushPrompts(cha)
        }
    }


    formated = formated.map((v) => {
        v.content = v.content.trim()
        return v
    })

    if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat){
        promptBodyformatedForChatStore = promptBodyformatedForChatStore.map((v) => {
            v.content = v.content.trim()
            return v
        })
    }


    if(currentChar.depth_prompt && currentChar.depth_prompt.prompt && currentChar.depth_prompt.prompt.length > 0){
        //depth_prompt
        const depthPrompt = currentChar.depth_prompt
        formated.splice(formated.length - depthPrompt.depth, 0, {
            role: 'system',
            content: risuChatParser(depthPrompt.prompt, {chara: currentChar})
        })
    }

    formated = await runLuaEditTrigger(currentChar, 'editRequest', formated)

    if(DBState.db.promptInfoInsideChat && DBState.db.promptTextInfoInsideChat){
        promptBodyformatedForChatStore = await runLuaEditTrigger(currentChar, 'editRequest', promptBodyformatedForChatStore)
        promptInfo.promptText = promptBodyformatedForChatStore
    }

    //token rechecking
    let inputTokens = 0

    for(const chat of formated){
        inputTokens += await tokenizer.tokenizeChat(chat)
    }

    if(inputTokens > maxContextTokens){
        let pointer = 0
        while(inputTokens > maxContextTokens){
            if(pointer >= formated.length){
                throwError(language.errors.toomuchtoken + "\n\nAt token rechecking. Required Tokens: " + inputTokens)
                return false
            }
            if(formated[pointer].removable){
                inputTokens -= await tokenizer.tokenizeChat(formated[pointer])
                formated[pointer].content = ''
            }
            pointer++
        }
        formated = formated.filter((v) => {
            return v.content !== ''  || (v.multimodals && v.multimodals.length > 0)
        })
    }

    //estimate tokens
    let outputTokens = DBState.db.maxResponse
    if(inputTokens + outputTokens > maxContextTokens){
        outputTokens = maxContextTokens - inputTokens
    }
    const generationId = v4()
    const generationModel = getGenerationModelString()

    generationInfo = {
        model: generationModel,
        generationId: generationId,
        inputTokens: inputTokens,
        outputTokens: outputTokens,
        maxContext: maxContextTokens,
        stageTiming: {
            stage1: stageTimings.stage1Duration,
            stage2: stageTimings.stage2Duration,
            stage3: 0,
            stage4: 0
        }
    }

    chatProcessStage.set(3)
    stageTimings.stage3Start = Date.now()
    if(arg.preview){
        previewFormated = formated
        return true
    }

    const requestSourceCharacterIndex = selectedChar
    const requestSourceCharacter = nowChatroom
    const requestSourceCharacterId = requestSourceCharacter.chaId
    const requestSourceChatPage = selectedChat
    const requestSourceConversation = requestSourceCharacter.chats[requestSourceChatPage]
    const requestSourceMessages = requestSourceConversation.message
    const requestSourceSession = getActiveConversationSession()
    const requestSourceSessionVersion = requestSourceSession?.version
    const isRequestSourceCurrent = () =>
        get(selectedCharID) === requestSourceCharacterIndex &&
        DBState.db.characters[requestSourceCharacterIndex]?.chaId === requestSourceCharacterId &&
        DBState.db.characters[requestSourceCharacterIndex]?.chatPage === requestSourceChatPage &&
        DBState.db.characters[requestSourceCharacterIndex]?.chats[requestSourceChatPage] ===
            requestSourceConversation &&
        getActiveConversationSession() === requestSourceSession &&
        (requestSourceSession === null
            ? requestSourceConversation.message === requestSourceMessages
            : requestSourceSession.isActive &&
              requestSourceSession.canContinueGenerationFrom(requestSourceSessionVersion!) &&
              requestSourceSession.materializeCompatibilityArray() ===
                  requestSourceConversation.message)
    const req = await requestChatData({
        formated: formated,
        biasString: biases,
        currentChar: currentChar,
        useStreaming: true,
        isGroupChat: nowChatroom.type === 'group',
        bias: {},
        continue: arg.continue,
        chatId: generationId,
        imageResponse: DBState.db.outputImageModal,
        previewBody: arg.previewPrompt,
        escape: nowChatroom.type === 'character' && nowChatroom.escapeOutput,
        rememberToolUsage: DBState.db.rememberToolUsage,
    }, 'model', abortSignal)
    if(!isRequestSourceCurrent()){
        return false
    }

    console.log(req)
    if(req.model){
        generationInfo.model = getGenerationModelString(req.model)
        console.log(generationInfo.model, req.model)
    }

    if(arg.previewPrompt && req.type === 'success'){
        previewBody = req.result
        return true
    }

    let result = ''
    let emoChanged = false
    let resendChat = false

    async function completeGeneration(): Promise<true> {
        if (lifecycle.reroll) return true
        await acknowledgeCompletedGeneration()
        await notifyGenerationCompletion(result)
        return true
    }

    async function acknowledgeCompletedGeneration(): Promise<void> {
        if (lifecycle.reroll) return
        lifecycle.acknowledgementAttempted = true
        await acknowledgeGenerationCompletion()
    }
    
    const responseApplication = await applyGenerationResponse({
        response: req,
        abortSignal,
        continueGeneration: arg.continue === true,
        sayingCharacterId: currentChar.chaId,
        generationId,
        generationInfo,
        promptInfo,
        removeIncompleteResponse: () => DBState.db.removeIncompleteResponse,
        streamingDisplayOptimizationMode: () =>
            DBState.db.streamingDisplayOptimizationMode ?? 'off',
        ttsAutoSpeech: () => DBState.db.ttsAutoSpeech,
        operation: {
            getCurrentSession: getActiveConversationSession,
            getTargetChat: () => DBState.db.characters[selectedChar]?.chats[selectedChat],
            isOwnerCurrent: () =>
                get(selectedCharID) === selectedChar &&
                DBState.db.characters[selectedChar]?.chaId === requestSourceCharacterId &&
                DBState.db.characters[selectedChar]?.chatPage === selectedChat,
            publishTargetChat: (chat) => {
                DBState.db.characters[selectedChar].chats[selectedChat] = chat
            },
            invalidateSession: invalidateActiveConversationSession,
            incrementReloadKeys: () => {
                DBState.db.characters[selectedChar].reloadKeys += 1
            },
        },
        callbacks: {
            reformatContent,
            processOutput: (data, messageIndex, processing) =>
                processing
                    ? processScriptFull(
                          nowChatroom,
                          reformatContent(data),
                          'editoutput',
                          messageIndex,
                          {},
                          processing,
                      )
                    : processScriptFull(
                          nowChatroom,
                          reformatContent(data),
                          'editoutput',
                          messageIndex,
                      ),
            runCurrentChatParser: runCurrentChatFunction,
            runInlay: (data) => runInlayScreen(currentChar, data),
            runOutputTrigger: (chat, onConversationCommit) =>
                runTrigger(currentChar, 'output', {
                    chat,
                    onConversationCommit,
                }),
            runOutputListeners: (chat, messageIndex) =>
                runChatOutputListeners(
                    currentChar,
                    chat,
                    selectedChar,
                    selectedChat,
                    messageIndex,
                    abortSignal,
                ),
            speak: async (data) => {
                try {
                    await sayTTS(currentChar, data)
                } catch (error) {
                    if (!lifecycle.reroll) throw error
                    alertError(error)
                }
            },
            addRerolls,
            trimIncompleteResponse: trimUntilPunctuation,
            markResponseApplied: () => {
                lifecycle.responseApplied = true
            },
            onProviderFailure: throwError,
        },
    })
    if (!responseApplication) return false
    lifecycle.responseCompleted = true
    result = responseApplication.result
    emoChanged = responseApplication.emoChanged
    resendChat = responseApplication.resendChat

    try {
    if(!responseApplication.hasOutputOwnership()){
        return false
    }
    let needsAutoContinue = false
    const resultTokens = await tokenize(result) + (arg.usedContinueTokens || 0)
    if(!responseApplication.hasOutputOwnership()){
        return false
    }
    if(DBState.db.autoContinueMinTokens > 0 && resultTokens < DBState.db.autoContinueMinTokens){
        needsAutoContinue = true
    }

    if(DBState.db.autoContinueChat && (!isLastCharPunctuation(result))){
        //if result doesn't end with punctuation or special characters, auto continue
        needsAutoContinue = true
    }

    if(needsAutoContinue){
        await acknowledgeCompletedGeneration()
        responseApplication.release()
        return await sendChat(chatProcessIndex, {
            chatAdditonalTokens: arg.chatAdditonalTokens,
            continue: true,
            signal: abortSignal,
            usedContinueTokens: resultTokens
        }, reservation)
    }

    const igp = risuChatParser(DBState.db.igpPrompt ?? "")

    if(igp){
        const igpFormated = parseChatML(igp)
        const rq = await requestChatData({
            formated: igpFormated,
            bias: {}
        },'emotion', abortSignal)

        if(!responseApplication.updateOutput((message) => ({
            ...message,
            data: message.data + rq,
        }))){
            return false
        }
    }

    stageTimings.stage3Duration = Date.now() - stageTimings.stage3Start

    if(generationInfo.stageTiming) {
        generationInfo.stageTiming.stage3 = stageTimings.stage3Duration
    }
    chatProcessStage.set(4)
    stageTimings.stage4Start = Date.now()

    if(resendChat){
        if(!responseApplication.hasOutputOwnership()){
            return false
        }
        stageTimings.stage4Duration = Date.now() - stageTimings.stage4Start
        
        if(generationInfo.stageTiming) {
            generationInfo.stageTiming.stage1 = stageTimings.stage1Duration
            generationInfo.stageTiming.stage2 = stageTimings.stage2Duration
            generationInfo.stageTiming.stage3 = stageTimings.stage3Duration
            generationInfo.stageTiming.stage4 = stageTimings.stage4Duration
        }
        
        const currentOutput = responseApplication.readOutput()
        if(currentOutput?.generationInfo && !responseApplication.updateOutput((message) => ({
            ...message,
            generationInfo,
        }))) {
            return false
        }

        await acknowledgeCompletedGeneration()
        responseApplication.release()
        return await sendChat(chatProcessIndex, {
            signal: abortSignal
        }, reservation)
    }

    if(req.special){
        if(req.special.emotion){
            let charemotions = get(CharEmotion)
            let currentEmotion = currentChar.emotionImages

            let tempEmotion = charemotions[currentChar.chaId]
            if(!tempEmotion){
                tempEmotion = []
            }
            if(tempEmotion.length > 4){
                tempEmotion.splice(0, 1)
            }

            for(const emo of currentEmotion){
                if(emo[0] === req.special.emotion){
                    const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                    tempEmotion.push(emos)
                    charemotions[currentChar.chaId] = tempEmotion
                    CharEmotion.set(charemotions)
                    emoChanged = true
                    break
                }
            }
        }
    }

    if(!currentChar.inlayViewScreen){
        if(currentChar.viewScreen === 'emotion' && (!emoChanged) && (abortSignal.aborted === false)){

            let currentEmotion = currentChar.emotionImages
            let emotionList = currentEmotion.map((a) => {
                return a[0]
            })
            let charemotions = get(CharEmotion)

            let tempEmotion = charemotions[currentChar.chaId]
            if(!tempEmotion){
                tempEmotion = []
            }
            if(tempEmotion.length > 4){
                tempEmotion.splice(0, 1)
            }

            if(DBState.db.emotionProcesser === 'embedding'){
                const hypaProcesser = new HypaProcesser()
                await hypaProcesser.addText(emotionList.map((v) => 'emotion:' + v))
                let searched = (await hypaProcesser.similaritySearchScored(result)).map((v) => {
                    v[0] = v[0].replace("emotion:",'')
                    return v
                })

                //give panaltys
                for(let i =0;i<tempEmotion.length;i++){
                    const emo = tempEmotion[i]
                    //give panalty index
                    const index = searched.findIndex((v) => {
                        return v[0] === emo[0]
                    })

                    const modifier = ((5 - ((tempEmotion.length - (i + 1))))) / 200

                    if(index !== -1){
                        searched[index][1] -= modifier
                    }
                }

                //make a sorted array by score
                const emoresult = searched.sort((a,b) => {
                    return b[1] - a[1]
                }).map((v) => {
                    return v[0]
                })

                for(const emo of currentEmotion){
                    if(emo[0] === emoresult[0]){
                        const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                        tempEmotion.push(emos)
                        charemotions[currentChar.chaId] = tempEmotion
                        CharEmotion.set(charemotions)
                        break
                    }
                }

                

                return await completeGeneration()
            }

            function shuffleArray(array:string[]) {
                for (let i = array.length - 1; i > 0; i--) {
                    const j = Math.floor(Math.random() * (i + 1));
                    [array[i], array[j]] = [array[j], array[i]];
                }
                return array
            }

            let emobias:{[key:number]:number} = {}

            for(const emo of emotionList){
                const tokens = await tokenizeNum(emo)
                for(const token of tokens){
                    emobias[token] = 10
                }
            }

            for(let i =0;i<tempEmotion.length;i++){
                const emo = tempEmotion[i]

                const tokens = await tokenizeNum(emo[0])
                const modifier = 20 - ((tempEmotion.length - (i + 1)) * (20/4))

                for(const token of tokens){
                    emobias[token] -= modifier
                    if(emobias[token] < -100){
                        emobias[token] = -100
                    }
                }
            }        

            const promptbody:OpenAIChat[] = [
                {
                    role:'system',
                    content: `${DBState.db.emotionPrompt2 || "From the list below, choose a word that best represents a character's outfit description, action, or emotion in their dialogue. Prioritize selecting words related to outfit first, then action, and lastly emotion. Print out the chosen word."}\n\n list: ${shuffleArray(emotionList).join(', ')} \noutput only one word.`
                },
                {
                    role: 'user',
                    content: `"Good morning, Master! Is there anything I can do for you today?"`
                },
                {
                    role: 'assistant',
                    content: 'happy'
                },
                {
                    role: 'user',
                    content: result
                },
            ]

            const rq = await requestChatData({
                formated: promptbody,
                bias: emobias,
                currentChar: currentChar,
                maxTokens: 30,
            }, 'emotion', abortSignal)

            if(rq.type === 'fail'){
                if(abortSignal.aborted){
                    return await completeGeneration()
                }
                throwError(rq.result)
                return await completeGeneration()
            }
            if(rq.type === 'streaming' || rq.type === 'multiline'){
                if(abortSignal.aborted){
                    return await completeGeneration()
                }
                throwError('Unexpected response type')
                return await completeGeneration()
            }
            else{
                emotionList = currentEmotion.map((a) => {
                    return a[0]
                })
                try {
                    const emotion:string = rq.result.replace(/ |\n/g,'').trim().toLocaleLowerCase()
                    let emotionSelected = false
                    for(const emo of currentEmotion){
                        if(emo[0] === emotion){
                            const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                            tempEmotion.push(emos)
                            charemotions[currentChar.chaId] = tempEmotion
                            CharEmotion.set(charemotions)
                            emotionSelected = true
                            break
                        }
                    }
                    if(!emotionSelected){
                        for(const emo of currentEmotion){
                            if(emotion.includes(emo[0])){
                                const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                                tempEmotion.push(emos)
                                charemotions[currentChar.chaId] = tempEmotion
                                CharEmotion.set(charemotions)
                                emotionSelected = true
                                break
                            }
                        }
                    }
                    if(!emotionSelected && emotionList.includes('neutral')){
                        const emo = currentEmotion[emotionList.indexOf('neutral')]
                        const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                        tempEmotion.push(emos)
                        charemotions[currentChar.chaId] = tempEmotion
                        CharEmotion.set(charemotions)
                        emotionSelected = true
                    }
                } catch (error) {
                    throwError(language.errors.httpError + `${error}`)
                    return await completeGeneration()
                }
            }
            
            return await completeGeneration()


        }
        else if(currentChar.viewScreen === 'imggen'){
            if(chatProcessIndex !== -1){
                throwError("Stable diffusion in group chat is not supported")
            }

            const msgs = DBState.db.characters[selectedChar].chats[selectedChat].message
            let msgStr = ''
            for(let i = (msgs.length - 1);i>=0;i--){
                if(msgs[i].role === 'char'){
                    msgStr = `character: ${msgs[i].data.replace(/\n/g, ' ')} \n` + msgStr
                }
                else{
                    msgStr = `user: ${msgs[i].data.replace(/\n/g, ' ')} \n` + msgStr
                    break
                }
            }


            await stableDiff(currentChar, msgStr)
        }
    }

    stageTimings.stage4Duration = Date.now() - stageTimings.stage4Start
    
    if(generationInfo.stageTiming) {
        generationInfo.stageTiming.stage1 = stageTimings.stage1Duration
        generationInfo.stageTiming.stage2 = stageTimings.stage2Duration
        generationInfo.stageTiming.stage3 = stageTimings.stage3Duration
        generationInfo.stageTiming.stage4 = stageTimings.stage4Duration
    }
    
    if(!responseApplication.hasOutputOwnership()){
        return false
    }
    const currentOutput = responseApplication.readOutput()
    if(currentOutput?.generationInfo && !responseApplication.updateOutput((message) => ({
        ...message,
        generationInfo,
    }))) {
        return false
    }

    return await completeGeneration()
    }
    finally {
        responseApplication.release()
    }
}

function systemizeChat(chat:OpenAIChat[]){
    for(let i=0;i<chat.length;i++){
        if(chat[i].role === 'user' || chat[i].role === 'assistant'){
            const attr = chat[i].attr ?? []
            if(chat[i].name?.startsWith('example_')){
                chat[i].content = chat[i].name + ': ' + chat[i].content
            }
            else if(!attr.includes('nameAdded')){
                chat[i].content = chat[i].role + ': ' + chat[i].content
            }
            chat[i].role = 'system'
            delete chat[i].memo
            delete chat[i].name
        }
    }
    return chat
}
