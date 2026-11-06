import { refuseGenerationWhilePluginsWait } from '../plugins/pluginStartupRefusal'
import { resolvePinnedConversationPosition } from '../plugins/pinnedConversationPosition'
import { registerGeneratingConversation } from '../storage/generatingConversationRegistry'
import { openGenerationRequestPhase } from './generationRequestPhase'
import { noteGenerationMessage, noteGenerationStarted } from './generationEnd'
import { boundedGenerationFallbackReason } from './boundedGenerationAdmission'
import { generationDiagnostics, type GenerationDiagnosticAdmission, type GenerationDiagnosticFallback, type GenerationDiagnosticOutcome, type GenerationDiagnosticPhase, type GenerationDiagnosticPhaseDetails, type GenerationDiagnosticTrace, type GenerationFailureReason, type GenerationSavePath } from '../generationDiagnostics'
import { get, writable } from "svelte/store";
import { type character, type MessageGenerationInfo, type Chat, type MessagePresetInfo, activatePresetOverride, setCurrentChat, type Message } from "../storage/database.svelte";
import { DBState } from '../stores.svelte';
import { CharEmotion, selectedCharID } from "../stores.svelte";
import { ChatTokenizer, encodeWithTokenizer, tokenize, tokenizeNum } from "../tokenizer";
import { language } from "../../lang";
import { alertError, alertToast, alertNormal } from "../alert";
import { parseChatML } from "../parser/chatML";
import { loadLoreBookV3Prompt } from "./lorebook.svelte";
import { effectiveAuthorNote } from "../authorNotePresets";
import { findCharacterbyId, getAuthorNoteDefaultText, getPersonaPrompt, getUserName, isLastCharPunctuation, trimUntilPunctuation, parseToggleSyntax, prebuiltAssetCommand } from "../util";
import { requestChatData } from "./request/request";
import { stableDiff } from "./stableDiff";
import { createPromptScriptOperationScope, processScript, processScriptFull, risuChatParser, type PromptScriptOperationScope } from "./scripts";
import { exampleMessage } from "./exampleMessages";
import { sayTTS } from "./tts";
import { supaMemory } from "./memory/supaMemory";
import { v4 } from "uuid";

import { runTrigger } from "./triggers";
import { HypaProcesser } from "./memory/hypamemory";
import { additionalInformations } from "./embedding/addinfo";
import { getInlayAsset } from "./files/inlays";
import { inlayImageForProvider } from "./files/inlayProviderImage";
import { getGenerationModelString } from "./models/modelString";

import { runInlayScreen } from "./inlayScreen";
import { runImageEmbedding } from "./transformers";
import { hanuraiMemory } from "./memory/hanuraiMemory";
import { hypaMemoryV2 } from "./memory/hypav2";
import { runLuaEditTrigger } from "./scriptings";
import { getModelInfo, LLMFlags } from "../model/modellist";
import { getCurrentHypaV3Preset, hypaMemoryV3 } from "./memory/hypav3";
import { getModuleAssets, getModuleLorebooks, getModuleRegexScripts, getModuleToggles, getModuleTriggers } from "./modules";
import { readImage } from "../globalApi.svelte";
import { pluginV2 } from "../plugins/plugins.svelte";
import { dispatchChatOutputListeners } from '../plugins/pluginChatOutputListeners'
import { createProductionPluginChatOutputProjector } from '../plugins/pluginDatabaseAccess'
import { activatePresetChainForRequest } from "./presetChain";
import {
    joinGenerationSignal,
    bindGenerationRegexOwner,
    reserveGeneration,
    type GenerationReservation,
} from './generationState'
import {
    acknowledgeGenerationCompletion,
    drainDeferredLwwReceives,
    assertPersistentMutationAllowed,
    getPersistentStorageAuthorityEpoch,
    getPersistentNavigationGeneration,
    acquireCompleteConversation,
    captureSelectedConversationAuthority,
    recordSelectedCharacterLastInteraction,
    captureWindowedConversationMutationController,
    captureSelectedConversationTarget,
    flushPendingData,
    flushPendingDataLocallyForCheckpoint,
    capturesImmutableCharacters,
    pendingConversationMutationDiagnostics,
    getActiveConversationSession,
    invalidateActiveConversationSession,
} from '../storage/persistentDataRuntime.svelte'
import { getPersistentDataStore } from '../storage/persistentDataStoreFactory'
import { ensureCurrentConversationMessageIds, writeConversationMetadata } from '../conversationMutations'
import { PersistentMutationFencedError } from '../storage/saveCoordinator'
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
    iteratePromptHistoryTargets,
    selectPromptHistory,
    type PromptHistoryCompatibilitySnapshot,
} from './promptHistory'
import { runCurrentChatParserPass } from './currentChatParserPass'
import {
    planSummaryAwareProcessedHistory,
    type SummaryAwarePromptHistoryPlan,
} from './summaryAwarePromptHistory'
import { applyGenerationErrorResponse } from './generationErrorResponse'
import { applyGenerationResponse } from './generationResponseApplication'
import { prepareSummaryAwareGeneration, type SummaryAwareGenerationPreparation } from './summaryAwareGenerationPreparation'
import { bindWindowedGenerationController } from './generationConversationOperation'
import {
    SelectedConversationPromotionStaleError,
    type CompleteConversationLease,
    type WindowedConversationMutationController,
} from '../storage/activeWorkingSet.svelte'
import { notifyIOSGenerationComplete } from "../iosNative";
import { isTauriIOS, isTauriAndroid, isTauriDesktop } from "../platform";
import { notifyDesktop } from "../desktopNotifications";
import { planChatParserHistory } from '../chatParserHistory'
import { getDeviceSettings } from '../storage/deviceSettings'
import { cloneConversationMetadata, isMetadataOnlySelectedConversation } from '../storage/selectedConversationLifecycle'
import {
    attachHistoryWindow,
    getHistoryWindowStart,
    registerActiveHistoryWindow,
    toAbsoluteIndex,
    toWindowIndex,
} from './historyWindowIndex'
import { WindowedConversationHistoryOperation } from './historyWindowHistory'
import {
    captureSessionHistoryWindowController,
    conversationFieldsOf,
    createHistoryWindowController,
    type HistoryWindowController,
    type HistoryWindowControllerEnvironment,
} from './historyWindowController'
import {
    admitHistoryWindow,
    createSessionHistoryWindowReader,
    createStoreHistoryWindowReader,
    prepareHistoryWindow,
    readStoreMessageMetadata,
    type HistoryWindowMemoryMode,
    type HistoryWindowPreparationInput,
} from './historyWindowGeneration'
import { readHistoryWindowTail, type HistoryWindowHypaPlan, type HistoryWindowReader } from './historyWindowSelection'
import { writeHistoryWindowChat } from './historyWindowWrite'

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

export async function runChatOutputListeners(char: any, chat: any, messageIndex: number, signal?: AbortSignal, diagnostics?: GenerationDiagnosticTrace){
    if(pluginV2.chatOutput.size === 0){
        return
    }
    await dispatchChatOutputListeners({
        listeners: pluginV2.chatOutput,
        char,
        chat,
        resolvePosition: resolvePinnedConversationPosition,
        messageIndex,
        windowStart: getHistoryWindowStart(chat) ?? 0,
        fullHistory: !getDeviceSettings().pluginOutputCurrentChatOnly,
        signal,
        projectScalable: projectPluginChatOutput,
        onPayload: diagnostics ? (payload) => diagnostics.listenerPayload(payload) : undefined,
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
    owner: character,
    chat: Chat,
): ConversationHistoryOperation {
    const session = getActiveConversationSession()
    if (session?.matchesConversation(owner.chaId, chat)) {
        return beginPinnedConversationHistoryOperation(session)
    }
    const snapshot = createCompatibilityConversationHistorySnapshot({
        characterId: owner.chaId,
        conversationId: chat.id ?? 'compatibility-current-conversation',
        messages: chat.message,
        storeRevision: session?.storeRevision ?? 0,
    })
    const windowStart = getHistoryWindowStart(chat)
    return windowStart === null
        ? snapshot
        : new WindowedConversationHistoryOperation(snapshot, windowStart)
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
    releaseGeneration?: () => void
    generationTarget?: { characterId: string; conversationId: string }
    authorityEpoch: number
    isTargetCurrent(): boolean
    onProgress?(completed: number): void
    responseCompleted: boolean
    reroll: boolean
    responseApplied: boolean
    acknowledgementAttempted: boolean
    diagnostics: GenerationDiagnosticTrace
    admission: GenerationDiagnosticAdmission
    diagnosticOutcome?: GenerationDiagnosticOutcome
    /** Why the open phase is failing; recorded only with a failed exit. */
    diagnosticReason?: GenerationFailureReason
    enterDiagnosticPhase(phase: GenerationDiagnosticPhase, details?: GenerationDiagnosticPhaseDetails): void
    finishDiagnosticPhase(outcome?: GenerationDiagnosticOutcome): void
    savePath(): GenerationSavePath
}

function preparationFailed(lifecycle: GenerationCompletionLifecycle, reason: GenerationFailureReason): false {
    lifecycle.diagnosticReason = reason
    return false
}

function diagnosticFallback(reason: string | null): GenerationDiagnosticFallback {
    switch (reason) {
        case null: return 'none'
        case 'selected-conversation-unavailable':
        case 'standard-hypa-v3-disabled':
        case 'selected-conversation-not-windowed':
        case 'custom-or-remote-tokenizer':
        case 'experimental-hypa-v3':
        case 'plugin-editprocess':
        case 'plugin-editoutput':
        case 'plugin-chat-output':
        case 'generation-trigger':
        case 'history-regex':
        case 'old-history-lorebook-consumer':
        case 'additional-text-consumer':
        case 'inlay-view-consumer':
        case 'dynamic-history-indirection':
        case 'explicit-history-reference': return reason
        default: return 'unknown'
    }
}

async function acknowledgeGenerationWithDiagnostics(lifecycle: GenerationCompletionLifecycle): Promise<void> {
    let savePath: GenerationSavePath = 'unknown'
    try {
        lifecycle.finishDiagnosticPhase()
        savePath = lifecycle.savePath()
        lifecycle.diagnostics.phase('final-acknowledgement', 'enter', { savePath })
    } catch {}
    let outcome: GenerationDiagnosticOutcome = 'failed'
    try {
        await acknowledgeGenerationCompletion(lifecycle.authorityEpoch)
        outcome = 'ok'
    } finally {
        try {
            lifecycle.diagnostics.phase('final-acknowledgement', 'exit', { outcome, savePath })
            const session = getActiveConversationSession()
            const matchingSession = session?.isActive
                && session.characterId === lifecycle.generationTarget?.characterId
                && session.conversationId === lifecycle.generationTarget?.conversationId
            const authorityCurrent = outcome === 'ok' && lifecycle.isTargetCurrent()
            const authority = authorityCurrent && !matchingSession ? captureSelectedConversationAuthority() : null
            const matchingAuthority = authority?.characterId === lifecycle.generationTarget?.characterId
                && authority?.conversationId === lifecycle.generationTarget?.conversationId
            lifecycle.diagnostics.persistence({
                boundary: 'final', savePath, ...pendingConversationMutationDiagnostics(),
                dirtyRecords: matchingSession ? session.pinCount('dirty') : undefined,
                acknowledgedRevision: !authorityCurrent ? undefined : matchingSession ? session.storeRevision
                    : matchingAuthority ? authority?.storeRevision : undefined,
            })
        } catch {}
    }
}

export async function notifyGenerationCompletion(result: string): Promise<void> {
    if (DBState.db.notification) {
        try {
            if (isTauriIOS) {
              await notifyIOSGenerationComplete();
            } else if (isTauriAndroid) {
              await window.RisuCompletionNotifications?.notify(result);
            } else if (isTauriDesktop) {
              await notifyDesktop(result);
            } else if (typeof Notification !== "undefined" && Notification.permission === "granted") {
              const notification = new Notification("RisuNest", {
                body: result,
              });
              notification.onclick = () => window.focus();
            }
        } catch {}
    }

}

interface GenerationConversationResources {
    completeLease: CompleteConversationLease | null
    preparation: SummaryAwareGenerationPreparation | null
    windowedController: WindowedConversationMutationController | null
    unbindWindowedController: (() => void) | null
}

interface GenerationHistoryWindow {
    totalMessages: number
    start: number
    endedAtAllBefore: boolean
    hypaPlan: HistoryWindowHypaPlan | null
    chat: Chat
    controller: HistoryWindowController
    characterId: string
    conversationId: string
    /** The metadata-only conversation in the database, when the conversation was windowed. */
    shell: Chat | null
}

/**
 * The memory mode a chat-screen send runs its history window with, or null
 * when the send loads the whole conversation.
 */
export function getHistoryWindowMemoryMode(historyLimit: boolean | undefined): HistoryWindowMemoryMode | null {
    if (historyLimit !== true) return null
    const owner = DBState.db.characters[get(selectedCharID)]
    if (!owner) return null
    return admitHistoryWindow({
        requested: true,
        enabled: getDeviceSettings().generationHistoryLimitEnabled,
        maxContext: DBState.db.maxContext,
        supaMemory: !!owner.supaMemory,
        supaModelType: DBState.db.supaModelType,
        hanuraiEnable: !!DBState.db.hanuraiEnable,
        hypav2: !!DBState.db.hypav2,
        hypaV3: !!DBState.db.hypaV3,
    })
}

function sameConversationTarget(
    left: { characterId: string, conversationId: string, navigationGeneration: number } | null,
    right: { characterId: string, conversationId: string, navigationGeneration: number },
): boolean {
    return left?.characterId === right.characterId
        && left.conversationId === right.conversationId
        && left.navigationGeneration === right.navigationGeneration
}

/**
 * Loads the newest part of the selected conversation for one generation and
 * returns a controller that writes it back by absolute position, whether the
 * conversation is windowed or complete. With `tailStart`, the window holds
 * the messages from the absolute index it returns instead of the token window.
 */
function createGenerationHistoryController(
    target: NonNullable<ReturnType<typeof captureSelectedConversationTarget>>,
    chat: Chat,
    start: number,
    initial: WindowedConversationMutationController,
    isTargetCurrent: () => boolean,
    signal: AbortSignal,
    requireUnchangedContent = false,
): HistoryWindowController {
    const currentConversation = () => {
        const currentOwner = DBState.db.characters[get(selectedCharID)]
        const current = currentOwner?.chats[currentOwner.chatPage]
        return currentOwner?.chaId === target.characterId && current?.id === target.conversationId
            ? { owner: currentOwner, conversation: current }
            : null
    }
    const environment: HistoryWindowControllerEnvironment = {
        captureWindowed: (windowChat, absoluteStartIndex) => {
            const current = captureSelectedConversationTarget()
            if (!isTargetCurrent() || !current || !sameConversationTarget(current, target)) return null
            return captureWindowedConversationMutationController(current, windowChat, absoluteStartIndex)
        },
        captureSession: () => {
            const current = isTargetCurrent() ? currentConversation() : null
            const session = getActiveConversationSession()
            if (requireUnchangedContent && current && session?.matchesConversation(current.owner.chaId, current.conversation)) {
                const fields = ['role', 'data', 'saying', 'chatId', 'name', 'disabled', 'isComment'] as const
                if (current.conversation.message.length !== start + chat.message.length
                    || chat.message.some((message, index) => fields.some((field) =>
                        !Object.is(message[field], current.conversation.message[start + index]?.[field])))) return null
            }
            return current && session?.matchesConversation(current.owner.chaId, current.conversation)
                ? { session, conversation: current.conversation }
                : null
        },
        getCurrentSession: getActiveConversationSession,
        readLiveMetadata: () => {
            const current = currentConversation()
            return current ? conversationFieldsOf(current.conversation) : null
        },
        onLost: () => {
            if (isTargetCurrent() && !signal.aborted) alertError(language.generationConversationChanged)
        },
    }
    return createHistoryWindowController(environment, chat, start, initial)
}

async function prepareGenerationHistoryWindow(
    initialTarget: NonNullable<ReturnType<typeof captureSelectedConversationTarget>>,
    memory: HistoryWindowMemoryMode,
    isTargetCurrent: () => boolean,
    signal: AbortSignal,
    tailStart?: (totalMessages: number) => number,
    retry = true,
): Promise<GenerationHistoryWindow | null> {
    let target = initialTarget
    let authority = captureSelectedConversationAuthority()
    if (authority && authority.sessionVersion !== authority.persistedSessionVersion) {
        await flushPendingData('generation-history-window')
        const current = captureSelectedConversationTarget()
        if (!isTargetCurrent() || !current || !sameConversationTarget(current, initialTarget)) return null
        target = current
        authority = captureSelectedConversationAuthority()
        if (authority && authority.sessionVersion !== authority.persistedSessionVersion) return null
    }
    const owner = DBState.db.characters[get(selectedCharID)]
    const conversation = owner?.chats[owner.chatPage]
    if (!owner || owner.chaId !== target.characterId || conversation?.id !== target.conversationId) return null
    const hypaSettings = memory === 'hypaV3' ? getCurrentHypaV3Preset().settings : null
    const preparation = {
        memory,
        conversation: cloneConversationMetadata(conversation),
        preserveOrphanedMemory: hypaSettings?.preserveOrphanedMemory ?? false,
        queryChatCount: hypaSettings?.queryChatCount ?? 0,
        tokenBudget: DBState.db.maxContext * getDeviceSettings().generationHistoryLimitMultiplier,
        // A remote or plugin tokenizer would see every older message, so the window walk counts locally.
        countTokens: hasObservableHistoryTokenizer()
            ? async (text: string) => (await encodeWithTokenizer(text, 'o200k_base')).length
            : tokenize,
    }
    const select = async (
        reader: HistoryWindowReader,
        readMetadata: HistoryWindowPreparationInput['readMetadata'],
        assertCurrent: () => void,
    ) => tailStart
        ? {
            selection: await readHistoryWindowTail(reader, tailStart(reader.totalMessages), assertCurrent),
            hypaPlan: null,
        }
        : prepareHistoryWindow({ ...preparation, reader, readMetadata, assertCurrent })

    let prepared: Awaited<ReturnType<typeof prepareHistoryWindow>>
    let chat: Chat
    let initial: WindowedConversationMutationController | null
    if (authority) {
        const windowedAuthority = authority
        const assertCurrent = () => {
            signal.throwIfAborted()
            const currentAuthority = captureSelectedConversationAuthority()
            if (
                !isTargetCurrent()
                || !sameConversationTarget(captureSelectedConversationTarget(), target)
                || currentAuthority?.sessionToken !== windowedAuthority.sessionToken
                || currentAuthority.storeRevision !== windowedAuthority.storeRevision
                || currentAuthority.sessionVersion !== windowedAuthority.sessionVersion
                || currentAuthority.totalMessages !== windowedAuthority.totalMessages
            ) throw new PersistentMutationFencedError()
        }
        try {
            const lease = await getPersistentDataStore().acquireRevision(windowedAuthority.storeRevision)
            try {
                prepared = await select(
                    createStoreHistoryWindowReader(lease, windowedAuthority),
                    () => readStoreMessageMetadata(lease, windowedAuthority, assertCurrent),
                    assertCurrent,
                )
            } finally {
                await lease.release()
            }
            assertCurrent()
        } catch (error) {
            if (retry && error instanceof PersistentMutationFencedError && !signal.aborted
                && isTargetCurrent() && sameConversationTarget(captureSelectedConversationTarget(), initialTarget)) {
                return prepareGenerationHistoryWindow(initialTarget, memory, isTargetCurrent, signal, tailStart, false)
            }
            throw error
        }
        chat = { ...preparation.conversation, message: prepared.selection.messages } as Chat
        initial = captureWindowedConversationMutationController(target, chat, prepared.selection.start)
    } else {
        const session = getActiveConversationSession()
        if (!session?.matchesConversation(owner.chaId, conversation)) return null
        const version = session.version
        const assertCurrent = () => {
            signal.throwIfAborted()
            if (
                !isTargetCurrent()
                || getActiveConversationSession() !== session
                || !session.isContentUnchangedSince(version)
            ) throw new PersistentMutationFencedError()
        }
        prepared = await select(
            createSessionHistoryWindowReader(session),
            async () => conversation.message.map((message) => ({
                chatId: message.chatId,
                disabled: message.disabled,
            })),
            assertCurrent,
        )
        assertCurrent()
        chat = { ...preparation.conversation, message: prepared.selection.messages } as Chat
        initial = captureSessionHistoryWindowController(
            { session, conversation },
            getActiveConversationSession,
            chat,
            prepared.selection.start,
        )
    }
    if (!initial) return null
    const start = prepared.selection.start

    const controller = createGenerationHistoryController(target, chat, start, initial, isTargetCurrent, signal)
    // Prompt history and scripts find messages by id, so missing ids are stored
    // first, one write per run of messages without one.
    for (let index = 0; index < chat.message.length;) {
        if (chat.message[index].chatId) {
            index += 1
            continue
        }
        let end = index + 1
        while (end < chat.message.length && !chat.message[end].chatId) end += 1
        const run = chat.message.slice(index, end).map((message) => ({ ...message, chatId: v4() }))
        if (!controller.applyRange(index, run.length, run, run.length === 1 ? 'edit' : 'replace-range')) {
            controller.release()
            return null
        }
        index = end
    }
    attachHistoryWindow(chat, start)
    return {
        start,
        totalMessages: prepared.selection.totalMessages,
        endedAtAllBefore: prepared.selection.endedAtAllBefore,
        hypaPlan: prepared.hypaPlan,
        chat,
        controller,
        characterId: target.characterId,
        conversationId: target.conversationId,
        shell: authority ? conversation : null,
    }
}

export interface SelectedHistoryWindow {
    readonly chat: Chat
    readonly controller: HistoryWindowController
    release(): void
}

/**
 * Opens a history window over the selected conversation for a chat-screen
 * step that runs outside generation: the token window, or with `tailStart`
 * the messages from the absolute index it returns. A registered window is
 * what CBS, Lua and regex read while the step runs. Returns null when the
 * conversation changed while the window was read.
 */
export async function openSelectedHistoryWindow(options: {
    tailStart?: (totalMessages: number) => number
    register?: boolean
    signal?: AbortSignal
} = {}): Promise<SelectedHistoryWindow | null> {
    const target = captureSelectedConversationTarget()
    if (!target) return null
    const authorityEpoch = getPersistentStorageAuthorityEpoch()
    const isTargetCurrent = () => {
        try {
            assertPersistentMutationAllowed(authorityEpoch)
        } catch (error) {
            if (error instanceof PersistentMutationFencedError) return false
            throw error
        }
        return sameConversationTarget(captureSelectedConversationTarget(), target)
    }
    let window: GenerationHistoryWindow | null
    try {
        window = await prepareGenerationHistoryWindow(
            target,
            'none',
            isTargetCurrent,
            options.signal ?? new AbortController().signal,
            options.tailStart,
        )
    } catch (error) {
        if (!(error instanceof PersistentMutationFencedError)) throw error
        window = null
    }
    if (!window) return null
    const unregister = options.register
        ? registerActiveHistoryWindow({
            characterId: window.characterId,
            conversationId: window.conversationId,
            shell: window.shell,
            controller: window.controller,
        })
        : null
    return {
        chat: window.chat,
        controller: window.controller,
        release() {
            unregister?.()
            window.controller.release()
        },
    }
}

function hasObservableHistoryTokenizer(): boolean {
    return !!(DBState.db.googleClaudeTokenizing
        || (DBState.db.aiModel === 'custom'
            && pluginV2.providerOptions?.get(DBState.db.currentPluginProvider)?.tokenizer === 'custom'))
}

export function getSelectedBoundedGenerationFallbackReason(): string | null {
    const owner = DBState.db.characters[get(selectedCharID)]
    const conversation = owner?.chats[owner.chatPage]
    const hypaSettings = owner?.supaMemory
        && DBState.db.hypaV3 && !DBState.db.hypav2 && !DBState.db.hanuraiEnable
        ? getCurrentHypaV3Preset().settings : null
    const authority = hypaSettings ? captureSelectedConversationAuthority() : null
    const scripts = hypaSettings
        ? [
            ...(DBState.db.presetRegex ?? []),
            ...(owner.customscript ?? []),
            ...getModuleRegexScripts(),
        ]
        : []
    const parserHistory = conversation && hypaSettings
        ? planChatParserHistory({
            source: [
                DBState.db.mainPrompt,
                DBState.db.additionalPrompt,
                DBState.db.globalNote,
                DBState.db.jailbreak,
                DBState.db.promptTemplate,
                owner.systemPrompt,
                owner.desc,
                owner.personality,
                owner.scenario,
                owner.replaceGlobalNote,
                owner.exampleMessage,
                owner.postHistoryInstructions,
                DBState.db.messageNameTemplate,
                DBState.db.promptSettings,
                owner.firstMessage,
                owner.alternateGreetings,
                conversation,
                getPersonaPrompt(),
            ],
            indirections: DBState.db.globalChatVariables,
        })
        : null
    return boundedGenerationFallbackReason({
        selected: !!owner && !!conversation,
        hypaEnabled: !!hypaSettings,
        windowed: !!authority,
        observableTokenizer: !!hasObservableHistoryTokenizer(),
        experimental: !!hypaSettings?.useExperimentalImpl,
        editprocess: (pluginV2.editprocess?.size ?? 0) > 0,
        editoutput: (pluginV2.editoutput?.size ?? 0) > 0,
        chatOutput: pluginV2.chatOutput.size > 0,
        triggers: ((owner?.triggerscript?.length ?? 0) > 0) || getModuleTriggers().length > 0,
        historyRegex: scripts.some(script => script.type === 'editprocess' || script.type === 'editoutput'),
        lorebooks: ((owner?.globalLore?.length ?? 0) > 0) || (conversation?.localLore?.length ?? 0) > 0 || getModuleLorebooks().length > 0,
        additionalText: !!owner?.additionalText,
        inlayView: !!owner?.inlayViewScreen,
        dynamicHistory: parserHistory?.requiresFullHistory !== false,
        explicitHistory: (parserHistory?.absoluteMessageIndices.length ?? 0) > 0,
    })
}

export async function sendChat(arg:{
    chatAdditonalTokens?:number,
    signal?:AbortSignal,
    continue?:boolean,
    usedContinueTokens?:number,
    preview?:boolean
    previewPrompt?:boolean
    /** A chat-screen send, which may load only recent messages when the device option is on. */
    historyLimit?:boolean
} = {}, inheritedReservation?: GenerationReservation):Promise<boolean> {
    if (refuseGenerationWhilePluginsWait()) return false
    const authorityEpoch = getPersistentStorageAuthorityEpoch()
    try {
        assertPersistentMutationAllowed(authorityEpoch)
    } catch (error) {
        if (error instanceof PersistentMutationFencedError) return false
        throw error
    }
    const initialCharacter = DBState.db.characters[get(selectedCharID)]
    const initialConversation = initialCharacter?.chats[initialCharacter.chatPage]
    const characterId = initialCharacter?.chaId
    const conversationId = initialConversation?.id
    const navigationGeneration = getPersistentNavigationGeneration()
    const isAuthorityCurrent = () => {
        try {
            assertPersistentMutationAllowed(authorityEpoch)
            return true
        } catch (error) {
            if (error instanceof PersistentMutationFencedError) return false
            throw error
        }
    }
    const isTargetCurrent = () => {
        if (!isAuthorityCurrent()) return false
        const currentCharacter = DBState.db.characters[get(selectedCharID)]
        const currentConversation = currentCharacter?.chats[currentCharacter.chatPage]
        return currentCharacter?.chaId === characterId &&
            getPersistentNavigationGeneration() === navigationGeneration &&
            (conversationId
                ? currentConversation?.id === conversationId
                : currentConversation === initialConversation)
    }
    const ownedReservation = inheritedReservation ? null : reserveGeneration()
    const reservation = inheritedReservation ?? ownedReservation
    if (!reservation?.isCurrent()) return false
    const conversationResources: GenerationConversationResources = {
        completeLease: null,
        preparation: null,
        windowedController: null,
        unbindWindowedController: null,
    }
    const diagnostics = generationDiagnostics.generation()
    const deviceSettings = getDeviceSettings()
    let activeDiagnosticPhase: GenerationDiagnosticPhase | null = null
    const lifecycle: GenerationCompletionLifecycle = {
        generationTarget: characterId && conversationId ? { characterId, conversationId } : undefined,
        authorityEpoch,
        isTargetCurrent,
        responseCompleted: false,
        reroll: !!DBState.db.characters[get(selectedCharID)]?.chats[
            DBState.db.characters[get(selectedCharID)]?.chatPage
        ]?.rerollRecovery,
        responseApplied: false,
        acknowledgementAttempted: false,
        diagnostics,
        admission: {
            intent: arg.continue ? 'continue' : initialConversation?.rerollRecovery ? 'reroll' : 'normal',
            displayMode: DBState.db.streamingDisplayOptimizationMode === 'balanced' ? 'normal'
                : DBState.db.streamingDisplayOptimizationMode === 'strong' ? 'strong' : 'off',
            profile: deviceSettings.performanceProfile === 'low-spec' ? 'low-spec' : 'normal',
            historyLimitEnabled: arg.historyLimit === true && deviceSettings.generationHistoryLimitEnabled,
            historyLimitMultiplier: deviceSettings.generationHistoryLimitMultiplier,
            route: 'unknown',
            totalMessages: initialConversation && !isMetadataOnlySelectedConversation(initialConversation)
                ? initialConversation.message.length : undefined,
        },
        enterDiagnosticPhase(phase, details) {
            lifecycle.finishDiagnosticPhase()
            activeDiagnosticPhase = phase
            diagnostics.phase(phase, 'enter', details)
        },
        finishDiagnosticPhase(outcome = 'ok') {
            if (activeDiagnosticPhase) diagnostics.phase(activeDiagnosticPhase, 'exit', {
                outcome, reason: outcome === 'failed' ? lifecycle.diagnosticReason : undefined,
            })
            activeDiagnosticPhase = null
        },
        savePath() {
            const session = getActiveConversationSession()
            if (session?.isActive && session.characterId === characterId && session.conversationId === conversationId) {
                return session.residencyFallbackActive ? 'fallback' : 'complete'
            }
            if (conversationResources.windowedController
                && initialConversation && isMetadataOnlySelectedConversation(initialConversation)) return 'windowed'
            return 'unknown'
        },
    }
    diagnostics.phase('send-admitted', 'enter')
    diagnostics.admission(lifecycle.admission)
    diagnostics.phase('send-admitted', 'exit', { outcome: 'ok' })
    let releaseGeneration: (() => void) | null = null
    let enteredGeneration = false
    let generationReturned = false
    const background = reservation.background
    let generationSignal: ReturnType<typeof joinGenerationSignal> | undefined
    try {
        lifecycle.enterDiagnosticPhase('history-materialization')
        const initialTarget = captureSelectedConversationTarget()
        const mayUseBoundedHistory = (DBState.db.hypaV3 || getHistoryWindowMemoryMode(arg.historyLimit) !== null)
            && initialTarget !== null
            && typeof captureSelectedConversationAuthority === 'function'
            && captureSelectedConversationAuthority() !== null
        if (initialTarget && !mayUseBoundedHistory) {
            try {
                conversationResources.completeLease = await acquireCompleteConversation(
                    'generation',
                    initialTarget,
                )
            } catch (error) {
                if (error instanceof SelectedConversationPromotionStaleError) { lifecycle.diagnosticOutcome = 'stale'; return false }
                throw error
            }
        }
        if (!lifecycle.isTargetCurrent()) return false
        enteredGeneration = true
        if (!arg.preview && !arg.previewPrompt) noteGenerationStarted(lifecycle.generationTarget)
        if (lifecycle.generationTarget) lifecycle.releaseGeneration = releaseGeneration = registerGeneratingConversation(lifecycle.generationTarget)
        generationSignal = joinGenerationSignal(arg.signal, background?.signal)
        if (background) lifecycle.onProgress = completed => background.progress(completed)
        const result = await sendChatInternal(
          { ...arg, signal: generationSignal.signal },
          lifecycle,
          reservation,
          conversationResources,
        );
        generationReturned = true
        lifecycle.diagnosticOutcome ??= result ? 'ok' : 'failed'
        return lifecycle.isTargetCurrent() && result
    } catch (error) {
        lifecycle.diagnosticReason ??= error instanceof PersistentMutationFencedError ? 'fenced' : 'error'
        if (generationSignal?.signal?.aborted || arg.signal?.aborted) {
            generationReturned = true
            return false
        }
        if (!arg.preview && (error instanceof PersistentMutationFencedError || !lifecycle.isTargetCurrent())) {
            generationReturned = true
            return false
        }
        if (lifecycle.reroll && lifecycle.responseCompleted && !arg.signal?.aborted) {
            alertError(error)
            return true
        }
        throw error
    } finally {
        let settledSavePath: GenerationSavePath = 'unknown'
        try {
            settledSavePath = lifecycle.savePath()
            lifecycle.finishDiagnosticPhase(generationSignal?.signal?.aborted || arg.signal?.aborted ? 'aborted'
                : !lifecycle.isTargetCurrent() ? 'stale' : lifecycle.diagnosticOutcome ?? 'failed')
        } catch {}
        try {
            releaseGeneration?.()
            // Receives held for the registered conversation are drained even when the
            // selection moved away during generation.
            if (lifecycle.generationTarget && !lifecycle.acknowledgementAttempted &&
                isAuthorityCurrent()) {
                try {
                    if (lifecycle.responseApplied) {
                        lifecycle.acknowledgementAttempted = true
                        await acknowledgeGenerationWithDiagnostics(lifecycle)
                        lifecycle.onProgress?.(3)
                    } else {
                        await drainDeferredLwwReceives(lifecycle.authorityEpoch)
                    }
                } catch (acknowledgeError) {
                    console.error(acknowledgeError)
                }
            }
            conversationResources.unbindWindowedController?.()
            conversationResources.windowedController?.release()
            await conversationResources.preparation?.release()
            conversationResources.completeLease?.release()
            generationSignal?.release()
            background?.settle(generationReturned && lifecycle.responseCompleted && !generationSignal?.signal?.aborted)
            if (background?.takeExpiry() && !arg.signal?.aborted) {
                alertNormal(language.generationInterruptedInBackground)
            }
            ownedReservation?.release({
                preserveBusy: enteredGeneration && !generationReturned,
            })
        } finally {
            try {
                const outcome = generationSignal?.signal?.aborted || arg.signal?.aborted ? 'aborted'
                    : !lifecycle.isTargetCurrent() ? 'stale' : lifecycle.diagnosticOutcome ?? 'failed'
                lifecycle.finishDiagnosticPhase(outcome)
                diagnostics.phase('settled', 'enter')
                diagnostics.phase('settled', 'exit', { outcome, savePath: settledSavePath })
            } catch {}
        }
    }
}

async function sendChatInternal(arg:{
    chatAdditonalTokens?:number,
    signal?:AbortSignal,
    continue?:boolean,
    usedContinueTokens?:number,
    preview?:boolean
    previewPrompt?:boolean
    historyLimit?:boolean
}, lifecycle: GenerationCompletionLifecycle, reservation: GenerationReservation,
conversationResources: GenerationConversationResources):Promise<boolean> {

    if (!lifecycle.isTargetCurrent()) return false
    chatProcessStage.set(0)
    const abortSignal = arg.signal ?? (new AbortController()).signal
    
    // NOTE: `throwError()` can be called before these are populated (e.g. HypaV3 early validation errors).
    // Keep them declared up-front to avoid TDZ ReferenceErrors in production builds.
    let selectedChar = -1
    let selectedChat = -1
    let currentChar:character
    let boundedChat: Chat | null = null
    let summaryAwareHistoryPlan: SummaryAwarePromptHistoryPlan | null = null
    let historyWindow: GenerationHistoryWindow | null = null
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
        if (!lifecycle.isTargetCurrent()) throw new PersistentMutationFencedError()
        const controller = conversationResources.windowedController
        if (controller?.chat !== chat) {
            return runCurrentChatParserPass({
                chat,
                database: DBState.db,
                ownerCharacterId: nowChatroom.chaId,
                parserCharacter: currentChar,
                session: controller ? null : getActiveConversationSession(),
                parser: risuChatParser,
            })
        }
        // The window is parsed in order without copying it, and the rows the parser changed are
        // written as one range, so a stale controller publishes none of them.
        const messages = chat.message.slice()
        const parsed = messages.map((message) => {
            const data = risuChatParser(message.data, { chara: currentChar, runVar: true })
            return data === message.data ? message : { ...message, data }
        })
        const start = parsed.findIndex((message, index) => message !== messages[index])
        if (start === -1) return chat
        let end = parsed.length
        while (parsed[end - 1] === messages[end - 1]) end--
        if (!controller.applyRange(start, end - start, parsed.slice(start, end), end - start === 1 ? 'edit' : 'replace-range')) {
            throw new PersistentMutationFencedError()
        }
        return chat
    }

    function reformatContent(data:string){
        return data.trim()
    }

    function throwError(error:string){
        if(!lifecycle.isTargetCurrent() || !DBState?.db?.inlayErrorResponse){
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
            const chatRoom = boundedChat ?? charRoom.chats?.[st]
            if(!chatRoom || !Array.isArray(chatRoom.message)){
                alertError(error)
                return
            }

            const suffix = `\n\`\`\`risuerror\n${error}\n\`\`\``
            const m:Message = {
                role: 'char',
                data: `\`\`\`risuerror\n${error}\n\`\`\``,
                time: Date.now(),
                chatId: v4(),
            }
            if(currentChar?.chaId){
                m.saying = currentChar.chaId
            }
            if(generationInfo){
                m.generationInfo = generationInfo
            }
            const session = getActiveConversationSession()
            const applied = applyGenerationErrorResponse({
                windowedController: boundedChat ? conversationResources.windowedController : null,
                session,
                getCurrentSession: getActiveConversationSession,
                characterId: charRoom.chaId,
                chat: chatRoom,
                getCurrentChat: () => {
                    if (boundedChat) {
                        return conversationResources.windowedController?.isCurrent()
                            ? boundedChat
                            : null
                    }
                    const currentOwner = DBState.db.characters?.[sc]
                    return currentOwner === charRoom && currentOwner.chatPage === st
                        ? currentOwner.chats?.[st]
                        : null
                },
                isOwnerCurrent: () => get(selectedCharID) === sc,
                suffix,
                appendMessage: m,
            })
            if (applied) noteGenerationMessage(lifecycle.generationTarget, chatRoom.message.at(-1)?.chatId)
            else alertError(error)
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

    await activatePresetChainForRequest(
        DBState.db,
        activatePresetOverride,
        Math.random,
        (name) => alertToast(`Cannot find preset: ${name}`),
    )



    if (
        !lifecycle.isTargetCurrent()
        || (generationSetupSession && getActiveConversationSession() !== generationSetupSession)
        || hasMismatchedActiveConversationSession()
    ) return false

    const target = captureSelectedConversationTarget()
    if (target) {
        const ownerIndex = get(selectedCharID)
        const owner = DBState.db.characters[ownerIndex]
        const conversation = owner?.chats[owner.chatPage]
        const hypaSettings = owner?.supaMemory
            && DBState.db.hypaV3 && !DBState.db.hypav2 && !DBState.db.hanuraiEnable
            ? getCurrentHypaV3Preset().settings
            : null
        const authority = hypaSettings ? captureSelectedConversationAuthority() : null
        const historyWindowMemory = getHistoryWindowMemoryMode(arg.historyLimit)
        const boundedFallbackReason = historyWindowMemory === null
            ? getSelectedBoundedGenerationFallbackReason()
            : 'history-window'
        const boundedAdmission = boundedFallbackReason === null
        lifecycle.admission.fallback = historyWindowMemory !== null ? 'none' : diagnosticFallback(boundedFallbackReason)
        if (authority) lifecycle.admission.totalMessages = authority.totalMessages
        if (historyWindowMemory !== null) {
            try {
                historyWindow = await prepareGenerationHistoryWindow(
                    target,
                    historyWindowMemory,
                    lifecycle.isTargetCurrent,
                    abortSignal,
                )
            } catch (error) {
                if (!(error instanceof PersistentMutationFencedError)) throw error
            }
            if (!historyWindow) {
                if (lifecycle.isTargetCurrent() && !abortSignal.aborted) {
                    alertError(language.chatConversationActionFailed)
                }
                return false
            }
            const unbind = bindWindowedGenerationController(historyWindow.chat, historyWindow.controller)
            const unregister = registerActiveHistoryWindow({
                characterId: historyWindow.characterId,
                conversationId: historyWindow.conversationId,
                shell: historyWindow.shell,
                controller: historyWindow.controller,
            })
            conversationResources.windowedController = historyWindow.controller
            conversationResources.unbindWindowedController = () => {
                unregister()
                unbind()
            }
            boundedChat = historyWindow.chat
            lifecycle.admission.route = 'history-window'
            lifecycle.admission.totalMessages = historyWindow.totalMessages
            console.debug('[Generation history] window', {
                start: historyWindow.start,
                messages: historyWindow.chat.message.length,
                memory: historyWindowMemory,
            })
        } else if (boundedAdmission) {
            const navigationGeneration = getPersistentNavigationGeneration()
            const isCurrent = () => {
                const currentTarget = captureSelectedConversationTarget()
                const currentAuthority = captureSelectedConversationAuthority()
                return lifecycle.isTargetCurrent()
                    && getPersistentNavigationGeneration() === navigationGeneration
                    && currentTarget?.characterId === target.characterId
                    && currentTarget.conversationId === target.conversationId
                    && currentTarget.storeRevision === target.storeRevision
                    && currentAuthority?.characterId === authority.characterId
                    && currentAuthority.conversationId === authority.conversationId
                    && currentAuthority.sessionToken === authority.sessionToken
                    && currentAuthority.storeRevision === authority.storeRevision
                    && currentAuthority.sessionVersion === authority.sessionVersion
                    && currentAuthority.persistedSessionVersion ===
                        authority.persistedSessionVersion
                    && currentAuthority.totalMessages === authority.totalMessages
            }
            const decision = await prepareSummaryAwareGeneration({
                store: getPersistentDataStore(),
                authority,
                conversation,
                preserveOrphanedMemory: hypaSettings.preserveOrphanedMemory,
                minimumTailMessages: hypaSettings.queryChatCount,
                signal: abortSignal,
                isCurrent,
            })
            if (decision.route === 'summary-aware') {
                if (arg.continue && decision.preparation.chat.message.length === 0) {
                    await decision.preparation.release()
                } else {
                const initial = captureWindowedConversationMutationController(
                    target,
                    decision.preparation.chat,
                    decision.preparation.plan.bodyStartIndex,
                )
                if (!initial) {
                    await decision.preparation.release()
                    return false
                }
                const controller = createGenerationHistoryController(
                    target, decision.preparation.chat, decision.preparation.plan.bodyStartIndex,
                    initial, lifecycle.isTargetCurrent, abortSignal, true,
                )
                conversationResources.preparation = decision.preparation
                conversationResources.windowedController = controller
                conversationResources.unbindWindowedController =
                    bindWindowedGenerationController(decision.preparation.chat, controller)
                boundedChat = decision.preparation.chat
                summaryAwareHistoryPlan = decision.preparation.plan
                lifecycle.admission.route = 'summary-aware'
                console.debug('[Generation history] bounded summary-aware', decision.preparation.metrics)
                }
            } else {
                lifecycle.admission.fallback = 'compatibility'
                console.debug('[Generation history] complete materialization', {
                    reason: decision.reason,
                })
            }
        } else if (hypaSettings) {
            console.debug('[Generation history] complete materialization', {
                reason: boundedFallbackReason,
            })
        }
        if (!boundedChat && !conversationResources.completeLease) {
            try {
                conversationResources.completeLease = await acquireCompleteConversation(
                    'generation',
                    target,
                )
            } catch (error) {
                if (error instanceof SelectedConversationPromotionStaleError) { lifecycle.diagnosticOutcome = 'stale'; return false }
                throw error
            }
        }
    }
    if (!lifecycle.isTargetCurrent()) return false

    const admittedConversation = boundedChat ?? DBState.db.characters[get(selectedCharID)]?.chats[
        DBState.db.characters[get(selectedCharID)]?.chatPage
    ]
    if (!boundedChat) lifecycle.admission.route = 'complete'
    lifecycle.admission.admittedMessages = admittedConversation?.message.length
    if (!boundedChat) lifecycle.admission.totalMessages = admittedConversation?.message.length
    lifecycle.diagnostics.admission(lifecycle.admission)
    lifecycle.enterDiagnosticPhase('prompt-preparation')

    DBState.db.statics.messages += 1
    selectedChar = get(selectedCharID)
    let nowChatroom = DBState.db.characters[selectedChar]
    const characterDetailWritable = () => {
        const owner = DBState.db.characters[selectedChar]
        const conversation = owner?.chats[owner.chatPage]
        return !conversation || !isMetadataOnlySelectedConversation(conversation)
    }
    const interactionTime = Date.now()
    const interactionAuthority = captureSelectedConversationAuthority()
    if (interactionAuthority) {
        if (interactionAuthority.characterId !== nowChatroom.chaId
            || interactionAuthority.conversationId !== nowChatroom.chats[nowChatroom.chatPage]?.id
            || !recordSelectedCharacterLastInteraction(interactionAuthority, nowChatroom.lastInteraction, interactionTime)) return preparationFailed(lifecycle, 'conversation-changed')
        nowChatroom.lastInteraction = interactionTime
    } else if (characterDetailWritable()) {
        nowChatroom.lastInteraction = interactionTime
    }
    selectedChat = nowChatroom.chatPage
    const selectedConversation = boundedChat ?? nowChatroom.chats[selectedChat]
    const activeSession = getActiveConversationSession()
    if (
        activeSession &&
        !activeSession.matchesConversation(
            nowChatroom.chaId,
            historyWindow ? nowChatroom.chats[selectedChat] : selectedConversation,
        )
    ) return preparationFailed(lifecycle, 'conversation-changed')
    if (!boundedChat) {
        ensureCurrentConversationMessageIds(
            nowChatroom,
            selectedConversation,
            activeSession,
            v4,
        )
    }
    
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

    currentChar = nowChatroom

    let chatAdditonalTokens = arg.chatAdditonalTokens ?? caculatedChatTokens
    const tokenizer = new ChatTokenizer(chatAdditonalTokens, DBState.db.aiModel.startsWith('gpt') ? 'noName' : 'name',
        true)
    let currentChat = runCurrentChatFunction(selectedConversation)
    if (!boundedChat) nowChatroom.chats[selectedChat] = currentChat
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

    const authorNote = effectiveAuthorNote(DBState.db.authorNotePresets, currentChat)
    if(authorNote){
        unformated.authorNote.push({
            role: 'system',
            content: risuChatParser(authorNote, {chara: currentChar})
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

    // A window that leaves out older messages leaves out the greeting, which the token trim would drop first.
    const greetingOutsideWindow = historyWindow !== null
        && historyWindow.start > 0
        && !historyWindow.endedAtAllBefore
    if(!greetingHistorySelection.resetByAllBefore && !greetingOutsideWindow){
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
    if (historyWindow && !historyWindow.controller.reconcileMetadata()) {
        throw new PersistentMutationFencedError()
    }
    bindGenerationRegexOwner(reservation, nowChatroom.chaId, currentChat.id)
    const triggerResult = await runTrigger(currentChar, 'start', {chat: currentChat, regexScope: reservation.regexScope, regexGeneration: abortSignal})
    if(triggerResult){
        if (historyWindow) {
            if (!writeHistoryWindowChat(historyWindow.controller, triggerResult.chat)) {
                throw new PersistentMutationFencedError()
            }
        } else if (triggerSessionSnapshot && triggerResult.chat !== currentChat) {
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
        if (!historyWindow) setCurrentChat(currentChat)
        currentTokens += triggerResult.tokens
        if(triggerResult.stopSending){
            return preparationFailed(lifecycle, 'trigger-stop')
        }
    }

    const requiresLivePromptCompatibility = (pluginV2.editprocess?.size ?? 0) > 0
    if (!historyWindow && !summaryAwareHistoryPlan && nowChatroom.supaMemory
        && DBState.db.hypaV3 && !DBState.db.hypav2 && !DBState.db.hanuraiEnable) {
        const settings = getCurrentHypaV3Preset().settings
        if (!settings.useExperimentalImpl && !requiresLivePromptCompatibility && !hasObservableHistoryTokenizer()
            && !(usingPromptTemplate && DBState.db.promptSettings.sendName)) {
            const decision = planSummaryAwareProcessedHistory(currentChat, [
                ...(DBState.db.presetRegex ?? []),
                ...(currentChar.customscript ?? []),
                ...getModuleRegexScripts(),
            ], settings.preserveOrphanedMemory, settings.queryChatCount, nowChatroom.chaId)
            if (decision.route === 'summary-aware') {
                summaryAwareHistoryPlan = decision.plan
                console.debug('[Generation history] complete view, summary-aware preprocessing', {
                    coveredMessages: decision.plan.coveredMessageIds.size,
                })
            } else console.debug('[Generation history] complete preprocessing', { reason: decision.reason })
        }
    }
    const promptHistory = beginPromptHistoryOperation(nowChatroom, currentChat)
    let promptHistoryCompatibilitySnapshot: PromptHistoryCompatibilitySnapshot | null = null
    let promptScriptOperationScope: PromptScriptOperationScope | null = null
    let preparedHistoryStartIndex = 0
    try {
    promptScriptOperationScope = createPromptScriptOperationScope(nowChatroom, {
        pluginCompatibility: requiresLivePromptCompatibility,
        historyWindow: historyWindow
            ? { chat: historyWindow.chat, conversationId: historyWindow.conversationId }
            : undefined,
    })
    const promptHistorySelection = selectPromptHistory(promptHistory)
    if (summaryAwareHistoryPlan) {
        if (conversationResources.windowedController) {
            if (!conversationResources.windowedController.isCurrent()) {
                throw new PersistentMutationFencedError()
            }
        }
    }
    preparedHistoryStartIndex = chats.length
    const promptHistoryEntries = requiresLivePromptCompatibility
        ? (promptHistoryCompatibilitySnapshot = createLivePromptHistoryCompatibilitySnapshot(
            currentChat.message,
            historyWindow
                ? {
                    ...promptHistorySelection,
                    startIndex: Math.max(toWindowIndex(currentChat, promptHistorySelection.startIndex), 0),
                    endIndex: toWindowIndex(currentChat, promptHistorySelection.endIndex),
                }
                : promptHistorySelection,
        )).entries
        : iteratePromptHistoryTargets(promptHistory, promptHistorySelection)
    for(const entry of promptHistoryEntries){
        promptScriptOperationScope.assertOwnerCurrent()
        const { relativeIndex: index } = entry
        let msg: Message
        if ('message' in entry) {
            ensurePromptHistoryMessageId(entry.message, v4)
            msg = entry.message
        } else {
            msg = ensurePromptHistoryEntryId(promptHistory, entry, v4)
        }
        if (msg.chatId && summaryAwareHistoryPlan?.coveredMessageIds.has(msg.chatId)) continue
        // Window messages carry stored ids and their locators belong to the window snapshot.
        if (!historyWindow) promptScriptOperationScope?.adoptMessageId(entry.locator, msg.chatId)
        const parsedMessage = promptScriptOperationScope
            ? promptScriptOperationScope.parse(nowChatroom, msg.data, {
                chara: currentChar,
                role: msg.role,
            })
            : risuChatParser(msg.data, {chara: currentChar, role: msg.role})
        // Scripts read and write window messages by their index in the whole conversation.
        const scriptIndex = !historyWindow
            ? index
            : requiresLivePromptCompatibility
            ? toAbsoluteIndex(currentChat, entry.absoluteIndex)
            : entry.absoluteIndex
        let formatedChat = (await processScriptFull(nowChatroom, parsedMessage, 'editprocess', scriptIndex, {
            chatRole: msg.role,
        }, {
            promptOperationScope: promptScriptOperationScope ?? undefined,
            regexGeneration: abortSignal,
            regexScope: reservation.regexScope,
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
                        const sendable = await inlayImageForProvider(inlayName, {
                            data: inlayData.data as string,
                            width: inlayData.width,
                            height: inlayData.height,
                        })
                        multimodal.push({
                            type: 'image',
                            base64: sendable.data,
                            width: sendable.width,
                            height: sendable.height
                        })
                    }
                    else{
                        const image = await inlayImageForProvider(inlayName, {
                            data: inlayData.data as string,
                            width: inlayData.width,
                            height: inlayData.height,
                        })
                        const captionResult = await runImageEmbedding(image.data)
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
            (usingPromptTemplate && DBState.db.promptSettings.sendName)
        ){
            const form = DBState.db.messageNameTemplate || `<{{char}}\'s Message>\n{{slot}}\n</{{char}}\'s Message>`
            const messageNameTemplate = promptScriptOperationScope
                ? promptScriptOperationScope.parse(nowChatroom, form, {
                    chara: findCharacterbyIdwithCache(msg.saying).name,
                })
                : risuChatParser(form, {chara: findCharacterbyIdwithCache(msg.saying).name})
            formatedChat = messageNameTemplate.replace('{{slot}}', formatedChat)
            switch(DBState.db.namedMessageRole){
                case 'user':
                case 'assistant':
                case 'system':
                    role = DBState.db.namedMessageRole
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
        try {
            historyWindow?.controller.reconcileMetadata()
        } catch {
            // The original error is the one to report.
        }
        throw error
    } finally {
        promptScriptOperationScope?.release()
        promptHistoryCompatibilitySnapshot?.dispose()
        promptHistory.dispose()
    }
    // Chat variables the prompt build set on the window chat are stored before the request.
    if (historyWindow && !historyWindow.controller.reconcileMetadata()) {
        throw new PersistentMutationFencedError()
    }

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
    
    // A session-owned chat records memory state through its session, so the save covers it.
    const writeChatMetadata = (chat: Chat, values: Partial<Omit<Chat, 'message'>>) =>
        writeConversationMetadata(nowChatroom, chat, getActiveConversationSession(), values)
    const writeMemoryMetadata = (values: Partial<Omit<Chat, 'message'>>) => {
        writeChatMetadata(currentChat, values)
        writeChatMetadata(DBState.db.characters[selectedChar].chats[selectedChat], values)
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
                return preparationFailed(lifecycle, 'memory-failed')
            }

            chats = hn.chats
            currentTokens = hn.tokens
        }
        else if(DBState.db.hypav2){
            console.log("Current chat's hypaV2 Data: ", currentChat.hypaV2Data)
            const sp = await hypaMemoryV2(chats, currentTokens, maxContextTokens, currentChat, nowChatroom, tokenizer)
            if(sp.error){
                if (abortSignal.aborted) return false
                console.log(sp)
                throwError(sp.error)
                return preparationFailed(lifecycle, 'memory-failed')
            }
            chats = sp.chats
            currentTokens = sp.currentTokens
            writeMemoryMetadata({ hypaV2Data: sp.memory ?? currentChat.hypaV2Data })

            currentChat = DBState.db.characters[selectedChar].chats[selectedChat];
            console.log("[Expected to be updated] chat's HypaV2Data: ", currentChat.hypaV2Data)
        }
        else if(DBState.db.hypaV3){
            console.log("Current chat's hypaV3 Data: ", currentChat.hypaV3Data)
            const sp = await hypaMemoryV3(
                chats,
                currentTokens,
                maxContextTokens,
                currentChat,
                nowChatroom,
                tokenizer,
                historyWindow?.hypaPlan ? {
                    effectiveMessageMemos: historyWindow.hypaPlan.effectiveMessageMemos,
                } : summaryAwareHistoryPlan ? {
                    boundaryMemo: summaryAwareHistoryPlan.boundaryMemo,
                    effectiveMessageMemos: summaryAwareHistoryPlan.effectiveMessageMemos,
                    historyStartIndex: preparedHistoryStartIndex,
                } : undefined,
                abortSignal,
            )
            if(sp.error){
                // Save new summary
                if (sp.memory) {
                    if (conversationResources.windowedController) {
                        currentChat.hypaV3Data = sp.memory
                        if (!conversationResources.windowedController.applyRange(
                            0, 0, [], 'update-metadata',
                        )) throw new PersistentMutationFencedError()
                    } else {
                        writeMemoryMetadata({ hypaV3Data: sp.memory })
                    }
                }
                if (abortSignal.aborted) return false
                console.log(sp)
                throwError(sp.error)
                return preparationFailed(lifecycle, 'memory-failed')
            }
            chats = sp.chats
            currentTokens = sp.currentTokens
            if (conversationResources.windowedController) {
                currentChat.hypaV3Data = sp.memory ?? currentChat.hypaV3Data
                if (!conversationResources.windowedController.applyRange(
                    0, 0, [], 'update-metadata',
                )) throw new PersistentMutationFencedError()
            } else {
                writeMemoryMetadata({ hypaV3Data: sp.memory ?? currentChat.hypaV3Data })
                currentChat = DBState.db.characters[selectedChar].chats[selectedChat];
            }
            if (abortSignal.aborted) return false
            console.log("[Expected to be updated] chat's HypaV3Data: ", currentChat.hypaV3Data)
        }
        else{
            const sp = await supaMemory(chats, currentTokens, maxContextTokens, currentChat, nowChatroom, tokenizer, {
                asHyper: DBState.db.hypaMemory
            })
            if(sp.error){
                throwError(sp.error)
                return preparationFailed(lifecycle, 'memory-failed')
            }
            chats = sp.chats
            currentTokens = sp.currentTokens
            writeMemoryMetadata({ supaMemoryData: sp.memory ?? currentChat.supaMemoryData })
            console.log(currentChat.supaMemoryData)
            writeChatMetadata(currentChat, { lastMemory: sp.lastId ?? currentChat.lastMemory })
        }
        stageTimings.stage2Duration = Date.now() - stageTimings.stage2Start
        chatProcessStage.set(1)
    }
    else{
        stageTimings.stage1Duration = Date.now() - stageTimings.stage1Start
        while(currentTokens > maxContextTokens){
            if(chats.length <= 1){
                throwError(language.errors.toomuchtoken + "\n\nRequired Tokens: " + currentTokens)

                return preparationFailed(lifecycle, 'token-limit')
            }

            currentTokens -= await tokenizer.tokenizeChat(chats[0])
            chats.splice(0, 1)
        }
        writeChatMetadata(currentChat, { lastMemory: chats[0].memo })
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
                return preparationFailed(lifecycle, 'token-limit')
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
    let requestSourceConversation = boundedChat
        ?? requestSourceCharacter.chats[requestSourceChatPage]
    let requestSourceShell = requestSourceCharacter.chats[requestSourceChatPage]
    let requestSourceMessages = requestSourceConversation.message
    if (!arg.previewPrompt) {
        noteGenerationMessage(lifecycle.generationTarget, generationId)
        if (arg.continue) noteGenerationMessage(lifecycle.generationTarget, requestSourceMessages.at(-1)?.chatId)
    }
    let requestSourceSession = getActiveConversationSession()
    let requestSourceSessionVersion = requestSourceSession?.version
    let requestSourceLost = false
    const isRequestSourceCurrent = () =>
        !requestSourceLost &&
        lifecycle.isTargetCurrent() &&
        get(selectedCharID) === requestSourceCharacterIndex &&
        DBState.db.characters[requestSourceCharacterIndex]?.chaId === requestSourceCharacterId &&
        DBState.db.characters[requestSourceCharacterIndex]?.chatPage === requestSourceChatPage &&
        (boundedChat && conversationResources.windowedController
            // The window controller follows a promotion and checks the window's message ids.
            ? conversationResources.windowedController.isCurrent()
            : DBState.db.characters[requestSourceCharacterIndex]?.chats[requestSourceChatPage] ===
                requestSourceShell &&
            (conversationResources.windowedController?.isCurrent() ?? true) &&
            getActiveConversationSession() === requestSourceSession &&
            (requestSourceSession === null
                ? requestSourceConversation.message === requestSourceMessages
                : requestSourceSession.isActive &&
                  requestSourceSession.canContinueGenerationFrom(requestSourceSessionVersion!) &&
                  requestSourceSession.materializeCompatibilityArray() ===
                      requestSourceConversation.message))
    const closeRequestPhase = openGenerationRequestPhase({
        characterId: requestSourceCharacterId,
        conversationId: requestSourceConversation.id ?? '',
    }, {
        prepare() {
            if (!isRequestSourceCurrent()) return null
            const session = getActiveConversationSession()
            if (!session) return null
            const version = session.version
            const invalidationVersion = session.generationInvalidationVersion
            return () => lifecycle.isTargetCurrent() && !abortSignal.aborted
                && getActiveConversationSession() === session
                && session.canContinueGenerationFrom(version)
                && session.generationInvalidationVersion === invalidationVersion
        },
        invalidate: () => { requestSourceLost = true },
        async adopt(receipt) {
            const target = captureSelectedConversationTarget()
            const expectedSession = receipt.session
            const expectedVersion = receipt.sessionVersion
            const receiptIsCurrent = () => expectedSession?.isActive
                && getActiveConversationSession() === expectedSession
                && expectedVersion !== null
                && expectedSession.canContinueGenerationFrom(expectedVersion)
                && expectedSession.generationInvalidationVersion === receipt.generationInvalidationVersion
                && getPersistentStorageAuthorityEpoch() === receipt.authorityEpoch
                && receipt.target && sameConversationTarget(captureSelectedConversationTarget(), receipt.target)
            if (!lifecycle.isTargetCurrent() || abortSignal.aborted || !target
                || !receiptIsCurrent()
                || target.characterId !== requestSourceCharacterId
                || target.conversationId !== requestSourceConversation.id) {
                requestSourceLost = true
                return
            }
            let lease: CompleteConversationLease
            try {
                lease = await acquireCompleteConversation('generation-public-write', target)
            } catch (error) {
                if (error instanceof SelectedConversationPromotionStaleError) { requestSourceLost = true; return }
                throw error
            }
            if (!lifecycle.isTargetCurrent() || abortSignal.aborted
                || lease.session !== expectedSession
                || !receiptIsCurrent()) {
                requestSourceLost = true
                lease.release()
                return
            }
            conversationResources.unbindWindowedController?.()
            conversationResources.unbindWindowedController = null
            conversationResources.windowedController?.release()
            conversationResources.windowedController = null
            conversationResources.completeLease?.release()
            conversationResources.completeLease = lease
            nowChatroom = DBState.db.characters[selectedChar]
            currentChar = nowChatroom
            currentChat = nowChatroom.chats[selectedChat]
            if (historyWindow) {
                // The public write promotes storage, but output scripts keep the admitted absolute window.
                const start = Math.min(historyWindow.start, currentChat.message.length)
                const chat = { ...cloneConversationMetadata(currentChat), message: safeStructuredClone(currentChat.message.slice(start)) } as Chat
                attachHistoryWindow(chat, start)
                const initial = captureSessionHistoryWindowController(
                    { session: lease.session, conversation: currentChat }, getActiveConversationSession, chat, start,
                )
                if (!initial) return
                const controller = createGenerationHistoryController(target, chat, start, initial, lifecycle.isTargetCurrent, abortSignal)
                historyWindow = { ...historyWindow, start, chat, controller, shell: null }
                boundedChat = chat
                conversationResources.windowedController = controller
                const unbind = bindWindowedGenerationController(chat, controller)
                const unregister = registerActiveHistoryWindow({
                    characterId: target.characterId, conversationId: target.conversationId, shell: null, controller,
                })
                conversationResources.unbindWindowedController = () => { unregister(); unbind() }
            } else {
                boundedChat = null
            }
            requestSourceConversation = boundedChat ?? currentChat
            requestSourceShell = currentChat
            requestSourceMessages = currentChat.message
            requestSourceSession = lease.session
            requestSourceSessionVersion = lease.session.version
        },
    })
    let req: Awaited<ReturnType<typeof requestChatData>>
    lifecycle.enterDiagnosticPhase('awaiting-provider')
    try {
        req = await requestChatData({
            formated: formated,
            biasString: biases,
            currentChar: currentChar,
            useStreaming: true,
            bias: {},
            continue: arg.continue,
            chatId: generationId,
            imageResponse: DBState.db.outputImageModal,
            previewBody: arg.previewPrompt,
            escape: nowChatroom.escapeOutput,
            rememberToolUsage: DBState.db.rememberToolUsage,
        }, 'model', abortSignal)
        lifecycle.finishDiagnosticPhase(abortSignal.aborted ? 'aborted' : req.type === 'fail' ? 'failed' : 'ok')
        const resultKind = req.type === 'success' ? 'buffered' : req.type === 'fail' ? 'failed' : req.type
        lifecycle.enterDiagnosticPhase('host-response', { resultKind })
    } finally {
        await closeRequestPhase()
    }
    const requestSourceCurrent = isRequestSourceCurrent()
    lifecycle.finishDiagnosticPhase(abortSignal.aborted ? 'aborted'
        : !requestSourceCurrent ? 'stale' : req.type === 'fail' ? 'failed' : 'ok')
    if(!requestSourceCurrent){
        lifecycle.diagnosticOutcome = 'stale'
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
        if (!lifecycle.isTargetCurrent()) throw new PersistentMutationFencedError()
        await notifyGenerationCompletion(result)
        return true
    }

    async function acknowledgeCompletedGeneration(): Promise<void> {
        if (lifecycle.reroll) return
        if (!lifecycle.isTargetCurrent()) throw new PersistentMutationFencedError()
        lifecycle.acknowledgementAttempted = true
        lifecycle.releaseGeneration?.()
        await acknowledgeGenerationWithDiagnostics(lifecycle)
        lifecycle.onProgress?.(3)
    }
    
    const responseApplication = await applyGenerationResponse({
        diagnostics: lifecycle.diagnostics,
        response: req,
        abortSignal,
        continueGeneration: arg.continue === true,
        sayingCharacterId: currentChar.chaId,
        ownerCharacterId: requestSourceCharacterId,
        generationId,
        generationInfo,
        promptInfo,
        removeIncompleteResponse: () => DBState.db.removeIncompleteResponse,
        streamingDisplayOptimizationMode: () =>
            DBState.db.streamingDisplayOptimizationMode ?? 'off',
        ttsAutoSpeech: () => DBState.db.ttsAutoSpeech,
        operation: {
            getCurrentSession: getActiveConversationSession,
            getTargetChat: () => boundedChat
                ?? DBState.db.characters[selectedChar]?.chats[selectedChat],
            isOwnerCurrent: () =>
                lifecycle.isTargetCurrent() &&
                get(selectedCharID) === selectedChar &&
                DBState.db.characters[selectedChar]?.chaId === requestSourceCharacterId &&
                DBState.db.characters[selectedChar]?.chatPage === selectedChat,
            publishTargetChat: (chat) => {
                if (!lifecycle.isTargetCurrent()) throw new PersistentMutationFencedError()
                if (boundedChat) {
                    if (chat === boundedChat) return
                    if (!historyWindow || !writeHistoryWindowChat(historyWindow.controller, chat)) {
                        throw new PersistentMutationFencedError()
                    }
                } else DBState.db.characters[selectedChar].chats[selectedChat] = chat
            },
            invalidateSession: () => {
                if (!boundedChat) invalidateActiveConversationSession()
            },
            incrementReloadKeys: () => {
                if (!lifecycle.isTargetCurrent()) throw new PersistentMutationFencedError()
                if (characterDetailWritable()) DBState.db.characters[selectedChar].reloadKeys += 1
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
                          { ...processing, regexScope: reservation.regexScope, regexGeneration: abortSignal },
                      )
                    : processScriptFull(
                          nowChatroom,
                          reformatContent(data),
                          'editoutput',
                          messageIndex,
                          {},
                          { regexScope: reservation.regexScope, regexGeneration: abortSignal },
                      ),
            runCurrentChatParser: runCurrentChatFunction,
            runInlay: (data) => runInlayScreen(currentChar, data),
            runOutputTrigger: (chat, onConversationCommit) =>
                runTrigger(currentChar, 'output', {
                    chat,
                    onConversationCommit,
                    regexGeneration: abortSignal,
                    regexScope: reservation.regexScope,
                }),
            runOutputListeners: (chat, messageIndex) =>
                runChatOutputListeners(
                    currentChar,
                    chat,
                    messageIndex,
                    abortSignal,
                    lifecycle.diagnostics,
                ),
            speak: async (data) => {
                try {
                    await sayTTS(currentChar, data)
                } catch (error) {
                    if (!lifecycle.reroll) throw error
                    alertError(error)
                }
            },
            trimIncompleteResponse: trimUntilPunctuation,
            markResponseApplied: () => {
                lifecycle.responseApplied = true
                lifecycle.onProgress?.(1)
            },
            onProviderFailure: throwError,
        },
        checkpoint: {
            savePath: () => lifecycle.savePath(),
            capturesImmutableCharacters,
            flush: () => flushPendingDataLocallyForCheckpoint('generation-checkpoint'),
        },
    })
    if (!responseApplication) {
        lifecycle.diagnosticOutcome = abortSignal.aborted ? 'aborted' : req.type === 'fail' ? 'failed' : 'stale'
        return false
    }
    lifecycle.responseCompleted = true
    lifecycle.onProgress?.(2)
    result = responseApplication.result
    emoChanged = responseApplication.emoChanged
    resendChat = responseApplication.resendChat
    lifecycle.enterDiagnosticPhase('completion-processing')

    try {
    if(!responseApplication.hasOutputOwnership()){
        lifecycle.diagnosticOutcome = 'stale'
        return false
    }
    let needsAutoContinue = false
    const resultTokens = await tokenize(result) + (arg.usedContinueTokens || 0)
    if(!responseApplication.hasOutputOwnership()){
        lifecycle.diagnosticOutcome = 'stale'
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
        return await sendChat({
            chatAdditonalTokens: arg.chatAdditonalTokens,
            continue: true,
            signal: abortSignal,
            usedContinueTokens: resultTokens,
            historyLimit: arg.historyLimit,
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
            lifecycle.diagnosticOutcome = 'stale'
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
            lifecycle.diagnosticOutcome = 'stale'
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
            lifecycle.diagnosticOutcome = 'stale'
            return false
        }

        await acknowledgeCompletedGeneration()
        responseApplication.release()
        return await sendChat({
            signal: abortSignal,
            historyLimit: arg.historyLimit,
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

            const msgs = (boundedChat ?? DBState.db.characters[selectedChar].chats[selectedChat]).message
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
        lifecycle.diagnosticOutcome = 'stale'
        return false
    }
    const currentOutput = responseApplication.readOutput()
    if(currentOutput?.generationInfo && !responseApplication.updateOutput((message) => ({
        ...message,
        generationInfo,
    }))) {
        lifecycle.diagnosticOutcome = 'stale'
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
