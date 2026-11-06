<script lang="ts">
    import { CbsHistoryAccessError } from 'src/ts/cbsHistoryAccess'
    import ResponseCandidateControls from './ResponseCandidateControls.svelte'
    import { activeRerollConversations } from 'src/ts/durableReroll'
    import { innerWidth } from 'svelte/reactivity/window'
    import { doingChat } from 'src/ts/process/index.svelte'
    import { ArrowLeft, ArrowLeftRightIcon, ArrowRight, BookmarkIcon, BotIcon, CopyIcon, PowerOff, GitBranch, HamburgerIcon, LanguagesIcon, MenuIcon, PencilIcon, RefreshCcwIcon, SplitIcon, TrashIcon, UserIcon, Volume2Icon, Scissors } from "@lucide/svelte"
    import { aiLawApplies, changeChatTo, foldChatToMessage, getFileSrc, createChatCopyName } from "src/ts/globalApi.svelte"
    import { ColorSchemeTypeStore } from "src/ts/gui/colorscheme"
    import { longpress } from "src/ts/gui/longtouch"
    import { closeTextEditorPopup, openTextEditorPopup } from "src/ts/gui/textEditorPopup.svelte"
    import { forgetEditorPopup, type ChatEditorDraft, type ChatEditorPopup, type ChatEditorPopupHandlers } from "src/ts/chatEditorDrafts"
    import { getModelInfo } from "src/ts/model/modellist"
    import { runLuaInteractionTrigger } from 'src/ts/process/scriptings'
    import { risuChatParser } from "src/ts/process/scripts"
    import { DisplayReadRecord, recordChatVariableReads, registerDisplayRow } from 'src/ts/process/scriptVariableReads'
    import { rebuildScriptDisplayRow } from 'src/ts/process/scriptDisplayRefresh'
    import { holdScriptRun } from 'src/ts/process/generationBackground'
    import { runTrigger } from 'src/ts/process/triggers'
    import { sayTTS } from "src/ts/process/tts"
    import { DBState, ReloadChatPointer, CurrentTriggerIdStore, popupStore } from 'src/ts/stores.svelte'

    import { capitalize, getUserIcon, getUserName, sleep } from "src/ts/util"
    import { onDestroy, onMount, tick, untrack } from "svelte"
    import { type Unsubscriber } from "svelte/store"
    import { v4 as uuidv4 } from 'uuid'
    import { language } from "../../lang"
    import { alertCheckboxConfirm, alertClear, alertConfirm, alertError, alertInput, alertNormal, alertRequestData, alertWait, alertToast } from "../../ts/alert"
    import { conversationMutationBlockedByGeneration } from 'src/ts/chatBindings.svelte'
    import { ParseMarkdown, type CbsConditions, type simpleCharacterArgument } from "../../ts/parser/parser.svelte"
    import { getCurrentCharacter, getCurrentChat, setCurrentChat, type character as CharacterRecord, type Message, type MessageGenerationInfo, type StreamingDisplayOptimizationMode } from "../../ts/storage/database.svelte"
    import { selectedCharID } from "../../ts/stores.svelte"
    import { HideIconStore, ReloadGUIPointer, selIdState } from "../../ts/stores.svelte"
    import AutoresizeArea from "../UI/GUI/TextAreaResizable.svelte"
    import ChatBody from './ChatBody.svelte'
    import { getStreamingThoughtPreview } from '../../ts/parser/streamingThoughtPreview'
    import { reportFailedBookmarkOperation } from '../Others/bookmarkOperation'
    import PopupButton from "../UI/PopupButton.svelte";
    import PluginDefinedIcon from '../Others/PluginDefinedIcon.svelte'
    import { createMessageButtonTarget, invokeMessageButton, messageButtonsForRole } from 'src/ts/plugins/messageButtons.svelte'
    import PartialEditController from './PartialEditController.svelte';
    import { getLLMCache, setLLMCache } from "../../ts/translator/translator"
    import { DeferredInlayMarkerRegistry, withResolvedDeferredInlaySources } from "src/ts/process/files/inlayRenderSource"
    import { copyImageSourceToDataUrl } from "src/ts/process/files/chatCopyInlays"
    import { getActiveConversationSession, getPersistentDataRuntime } from "../../ts/storage/persistentDataRuntime.svelte"
    import { removeChatMessage } from "../../ts/chatRemoval"
    import { createCapturedConversationBranch } from "../../ts/chatBranchUi"
    import {
        captureChatMessageTarget,
        resolveChatMessageTarget,
        saveCapturedChatMessage,
        toggleCapturedBookmark,
        toggleCapturedMessageDisabled,
        toggleCapturedMessageRole,
        type CapturedChatMessageTarget,
    } from "../../ts/chatMessageUi"
    import type { DeepReadonly, FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import { safeStructuredClone } from 'src/ts/polyfill'
    import isEqual from 'lodash/isEqual'
    import type { ConversationViewportRow } from 'src/ts/conversationViewportSource'
    import type { ChatDisplayRefresh, ChatPresentationRefresh } from 'src/ts/chatDisplayRefresh'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type {
        SelectedConversationMessageEditIntent,
        SelectedConversationOperations,
    } from 'src/ts/selectedConversationOperations'
    import { SelectedConversationPromotionStaleError } from 'src/ts/storage/activeWorkingSet.svelte'

    let translating = $state(false)
    let editMode = $state(false)
    // The draft is edited in the popup editor, so the row keeps showing the message.
    let editInPopup = $state(false)
    let inlineEditMode = $derived(editMode && !editInPopup)
    let statusMessage:string = $state('')
    let retranslate = $state(false)
    let editTranslationMode = $state(false)
    let loadingTranslationEdit = $state(false)
    let editTranslationText = $state('')
    let editTranslationKey: string | null = null
    // A restored translation editor saves only while its cache key still names the same text.
    let restoredTranslation = false
    let translationEvidence: Message | null = null
    let editPopup: ChatEditorPopup | null = null
    let chatBodyRevision = $state(0)
    // Explicit refreshes replace this so the message is parsed once per flush.
    // Assigning without reading keeps a caller's effect from depending on it.
    let displayRequest = $state.raw({})
    let bodyRoot:HTMLElement|null = $state(null)
    let editTarget: CapturedChatMessageTarget | null = null
    let editEvidence: Message | null = null
    let partialEditTarget = $state.raw<CapturedChatMessageTarget | null>(null)
    let editIntent: SelectedConversationMessageEditIntent | null = null
    let partialEditIntent = $state.raw<SelectedConversationMessageEditIntent | null>(null)
    let partialEditController = $state<PartialEditController | undefined>()
    interface AcquiredChatMessageTarget {
        readonly target: CapturedChatMessageTarget
        release(): void
    }
    interface Props {
        message?: string
        name?: string
        largePortrait?: boolean
        isLastMemory: boolean
        img?: string | Promise<string>
        idx?: number
        messageGenerationInfo?: MessageGenerationInfo | null
        rerollIcon?: boolean | 'dynamic'
        role?: string
        totalLength?: number
        onReroll?: () => void
        onNextReroll?: () => void
        unReroll?: () => void
        character?: simpleCharacterArgument | string | null
        firstMessage?: boolean
        altGreeting?: boolean
        currentPage?: number
        totalPages?: number
        isComment?: boolean
        disabled?: boolean | 'allBefore'
        isOptimizedStreamingMessage?: boolean
        streamingOptimizationMode?: StreamingDisplayOptimizationMode
        rawStreamingText?: string
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
        captureContext?: FrozenChatScreenshotRenderContext
        captureMessage?: DeepReadonly<Message>
        captureParserIndex?: number
        viewportRow?: ConversationViewportRow
        viewportSourceToken?: string
        captureViewportTarget?: () => CapturedChatMessageTarget | null
        selectedConversationOperations?: SelectedConversationOperations
        bookmarked?: boolean
        parserProjection?: BoundedLiveChatParserProjection
        parserAbortSignal?: AbortSignal
        onDisplaySettled?: () => void
        /** When the view asked for this row, from `nextDisplayListenerOrder`. */
        displayRequestOrder?: number
        restoredEditor?: ChatEditorDraft
        onEditorOpen?: () => void
        onEditorClose?: () => void
        onBodyRendered?: () => void
    }

    let {
        message = $bindable(''),
        name = '',
        largePortrait = false,
        isLastMemory,
        img = '',
        idx = -1,
        rerollIcon = false,
        messageGenerationInfo = null,
        role = null,
        totalLength = 0,
        onReroll = () => {},
        onNextReroll = onReroll,
        unReroll = () => {},
        character = null,
        firstMessage = false,
        altGreeting = false,
        currentPage = 1,
        totalPages = 1,
        isComment = false,
        disabled = false,
        isOptimizedStreamingMessage = false,
        streamingOptimizationMode = 'off',
        rawStreamingText = message,
        onCaptureSettled,
        onCaptureError,
        captureContext,
        captureMessage,
        captureParserIndex = idx,
        viewportRow,
        viewportSourceToken,
        captureViewportTarget,
        selectedConversationOperations,
        bookmarked,
        parserProjection,
        parserAbortSignal,
        onDisplaySettled,
        displayRequestOrder,
        restoredEditor,
        onEditorOpen,
        onEditorClose,
        onBodyRendered,
    }: Props = $props()

    let editDraft = $state(message)
    let captureSettings = $derived(captureContext?.settings)
    let captureCharacter = $derived(captureContext?.parserContext.character)
    let captureChat = $derived(captureCharacter?.chats[captureCharacter.chatPage])
    let retainedLayout: { theme: string; gui: HTMLElement | null } | undefined
    let presentedLayout = $derived.by(() => {
        if (hasActiveEditor() && retainedLayout) return retainedLayout
        const theme = captureSettings?.theme ?? DBState.db.theme
        const gui = theme === 'customHTML' ? RenderGUIHtml(captureSettings?.guiHTML ?? DBState.db.guiHTML) : null
        retainedLayout = { theme, gui }
        return retainedLayout
    })
    let captureTheme = $derived(presentedLayout.theme)
    let captureIconSize = $derived(captureSettings?.iconSize ?? DBState.db.iconsize)
    let captureZoomSize = $derived(captureSettings?.zoomSize ?? DBState.db.zoomsize)
    let captureLineHeight = $derived(captureSettings?.lineHeight ?? DBState.db.lineHeight ?? 1.25)
    let hideCaptureIcon = $derived(captureSettings?.hideIcons ?? $HideIconStore)
    let presentedMessage = $derived(captureMessage ?? viewportRow?.message)
    let presentedChatId = $derived(
        captureContext
            ? (captureMessage?.chatId ?? '')
            : viewportRow
              ? (viewportRow.message.chatId ?? '')
              : idx < 0
                ? ''
                : (DBState.db.characters?.[selIdState.selId]?.chats?.[
                      DBState.db.characters?.[selIdState.selId]?.chatPage
                  ]?.message?.[idx]?.chatId ?? ''),
    )
    let presentedTime = $derived(
        captureContext
            ? captureMessage?.time
            : viewportRow
              ? viewportRow.message.time
              : idx < 0
                ? undefined
                : DBState.db.characters?.[selIdState.selId]?.chats?.[
                      DBState.db.characters?.[selIdState.selId]?.chatPage
                  ]?.message?.[idx]?.time,
    )
    let presentedRole = $derived(
        captureContext
            ? (captureMessage?.role ?? role)
            : viewportRow
              ? viewportRow.message.role
              : idx < 0
                ? role
                : (DBState.db.characters?.[selIdState.selId]?.chats?.[
                      DBState.db.characters?.[selIdState.selId]?.chatPage
                  ]?.message?.[idx]?.role ?? role),
    )

    function parserContext() {
        return captureContext?.parserContext ?? parserProjection?.context.parserContext
    }

    function parserChara(): string | CharacterRecord {
        const character = parserContext()?.character as CharacterRecord
        return character
    }

    function parserArgs() {
        const parser = parserContext()
        if (!parser) return {}
        return {
            db: parser.database,
            chara: parserChara(),
            chatID: idx,
            projectedChatID: captureContext
                ? captureParserIndex
                : parserProjection?.projectedChatID,
            historyOffset: parser.historyOffset,
            historyAccess: parser.historyAccess,
            userName: parser.userName,
            personaPrompt: parser.personaPrompt,
            modules: parser.modules,
            moduleLorebooks: parser.moduleLorebooks,
            selectedCharID: parser.selectedCharID,
            chatVariables: parser.chatVariables,
            globalChatVariables: parser.globalChatVariables,
            currentTime: parser.currentTime,
            triggerId: parser.triggerId,
            role: captureMessage?.role ?? presentedRole,
        } as unknown as Parameters<typeof risuChatParser>[1]
    }

    // Script variable changes refresh this row only when its display read a changed variable.
    const displayRow = untrack(() => captureContext) ? null : registerDisplayRow(() => idx)
    let msgDisplay = $state('')
    let translated = $state(false)
    let partialEditEnabled = $state(true)
    let translationViewControlsDisabled = $derived(editMode || editTranslationMode || loadingTranslationEdit)
    let originalEditControlDisabled = $derived(editTranslationMode || loadingTranslationEdit)
    let translationEditControlDisabled = $derived(editMode || loadingTranslationEdit)

    export function updateCandidatePosition(page: number, total: number) {
        currentPage = page
        totalPages = total
    }

    export function updateStreamingDisplay(state: {
        isOptimizedStreamingMessage: boolean
        streamingOptimizationMode: StreamingDisplayOptimizationMode
        rawStreamingText: string
    }){
        isOptimizedStreamingMessage = state.isOptimizedStreamingMessage
        streamingOptimizationMode = state.streamingOptimizationMode
        rawStreamingText = state.rawStreamingText
    }

    export function updateViewportBinding(state: {
        viewportRow: ConversationViewportRow
        viewportSourceToken: string
        captureViewportTarget: () => CapturedChatMessageTarget | null
        parserProjection?: BoundedLiveChatParserProjection
        totalMessages: number
        preserveDisplay?: boolean
    }) {
        viewportRow = state.viewportRow
        viewportSourceToken = state.viewportSourceToken
        captureViewportTarget = state.captureViewportTarget
        if (!state.preserveDisplay) parserProjection = state.parserProjection
        idx = state.viewportRow.absoluteIndex
        totalLength = state.totalMessages
        if (selectedConversationOperations) {
            const input = {
                absoluteIndex: viewportRow.absoluteIndex,
                sourceToken: viewportSourceToken,
                sourceVersion: viewportRow.sourceVersion,
                rowKey: viewportRow.key,
                message: viewportRow.message,
            }
            if (editIntent) editIntent = selectedConversationOperations.rebindMessageEditIntent(editIntent, input)
            if (partialEditIntent) partialEditIntent = selectedConversationOperations.rebindMessageEditIntent(partialEditIntent, input)
        }
        if (!state.preserveDisplay) displayRequest = {}
    }

    export function refreshParserProjection(
        projection?: BoundedLiveChatParserProjection,
    ) {
        parserProjection = projection
    }

    export function hasActiveEditor(): boolean {
        return (
            editMode ||
            editTranslationMode ||
            loadingTranslationEdit ||
            partialEditIntent !== null ||
            partialEditTarget !== null
        )
    }

    let editorWasActive = false
    $effect(() => {
        const active = hasActiveEditor()
        if (editorWasActive && !active) untrack(() => onEditorClose?.())
        editorWasActive = active
    })

    export function updatePresentation(state: ChatPresentationRefresh): void {
        img = state.img
        name = state.name
        largePortrait = state.largePortrait
        bookmarked = state.bookmarked
        role = state.role
        messageGenerationInfo = state.messageGenerationInfo ?? null
    }

    export function captureEditorDraft(): Omit<ChatEditorDraft, 'caret'> | null {
        if (editMode) {
            const evidence = editIntent?.messageEvidence ?? editEvidence
            if (!evidence) return null
            return { kind: 'original', draft: editDraft, index: idx, evidence, popup: editInPopup ? editPopup ?? undefined : undefined }
        }
        if (editTranslationMode && editTranslationKey !== null && translationEvidence) {
            return { kind: 'translation', draft: editTranslationText, index: idx, evidence: translationEvidence, translationKey: editTranslationKey }
        }
        return null
    }

    export function restoreEditor(restored: ChatEditorDraft) {
        if (restored.kind === 'translation') {
            if (restored.translationKey === undefined) return
            editTranslationKey = restored.translationKey
            editTranslationText = restored.draft
            translationEvidence = safeStructuredClone(restored.evidence) as Message
            restoredTranslation = true
            translated = true
            editTranslationMode = true
            return
        }
        editDraft = restored.draft
        editMode = captureEditTarget()
        if (!restored.popup) return
        if (!editMode) {
            closeTextEditorPopup(restored.popup.request)
            return
        }
        editInPopup = true
        editPopup = restored.popup
        restored.popup.owner = popupHandlers()
    }

    function reportRowActionRefused(): false {
        alertToast(language.chatMessageActionFailed)
        return false
    }

    function mutationBlocked(): boolean {
        return conversationMutationBlockedByGeneration(captureCurrentChat()?.conversation.id)
    }

    export function refreshMessageDisplay(state: ChatDisplayRefresh): void {
        message = state.message
        if (state.index !== undefined) idx = state.index
        if (state.character !== undefined) character = state.character
        totalLength = state.totalMessages
        parserProjection = state.parserProjection
        parserAbortSignal = state.parserAbortSignal
        onDisplaySettled = state.onDisplaySettled
        if (state.displayRequestOrder !== undefined) displayRequestOrder = state.displayRequestOrder
        if (state.viewportBinding) {
            updateViewportBinding({
                ...state.viewportBinding,
                parserProjection: state.parserProjection,
                totalMessages: state.totalMessages,
            })
        }
        // Complete-history renders have no bounded projection object to change.
        // They still need to re-evaluate scripts when any message changes.
        chatBodyRevision += 1
        displayRequest = {}
    }

    function currentConversationSession() {
        const currentCharacter = DBState.db.characters[selIdState.selId]
        const currentChat = currentCharacter?.chats[currentCharacter.chatPage]
        const session = getActiveConversationSession()
        return currentCharacter && currentChat &&
            session?.matchesConversation(currentCharacter.chaId, currentChat)
            ? session
            : null
    }

    function captureCurrentChat() {
        const character = DBState.db.characters[selIdState.selId]
        const conversation = character?.chats[character.chatPage]
        return character && conversation ? { character, conversation } : null
    }

    const chatMessageContext = {
        captureCurrent: captureCurrentChat,
        getCurrentSession: currentConversationSession,
    }

    function captureCurrentMessage() {
        if (captureViewportTarget) return captureViewportTarget()
        return captureChatMessageTarget({
            ...chatMessageContext,
            absoluteIndex: idx,
        })
    }

    async function acquireCurrentMessage(
        reason: string,
    ): Promise<AcquiredChatMessageTarget | null> {
        if (viewportRow && selectedConversationOperations) {
            try {
                if (viewportSourceToken && ['toggle-bookmark', 'toggle-message-disabled', 'toggle-messages-above-disabled', 'toggle-message-role'].includes(reason)) {
                    return await selectedConversationOperations.acquireMessageMutation({
                        absoluteIndex: viewportRow.absoluteIndex, sourceToken: viewportSourceToken,
                        sourceVersion: viewportRow.sourceVersion, rowKey: viewportRow.key, message: viewportRow.message,
                    }, reason)
                }
                return await selectedConversationOperations.acquireCompleteMessageTarget(
                    viewportRow.absoluteIndex,
                    reason,
                )
            } catch (error) {
                if (error instanceof SelectedConversationPromotionStaleError) return null
                throw error
            }
        }
        const target = captureCurrentMessage()
        return target ? { target, release() {} } : null
    }

    function captureViewportEditIntent(): SelectedConversationMessageEditIntent | null {
        if (!viewportRow || !viewportSourceToken || !selectedConversationOperations) return null
        return selectedConversationOperations.captureMessageEditIntent({
            absoluteIndex: viewportRow.absoluteIndex,
            sourceToken: viewportSourceToken,
            sourceVersion: viewportRow.sourceVersion,
            rowKey: viewportRow.key,
            message: viewportRow.message,
        })
    }

    async function acquireEditMessage(
        intent: SelectedConversationMessageEditIntent | null,
        target: CapturedChatMessageTarget | null,
        reason: string,
    ): Promise<AcquiredChatMessageTarget | null> {
        if (intent && selectedConversationOperations) {
            try {
                return await selectedConversationOperations.acquireCompleteMessageTargetForIntent(
                    intent,
                    reason,
                )
            } catch (error) {
                if (error instanceof SelectedConversationPromotionStaleError) return null
                throw error
            }
        }
        return target ? { target, release() {} } : null
    }

    function captureEditTarget(): boolean {
        editIntent = captureViewportEditIntent()
        editTarget = editIntent ? null : captureCurrentMessage()
        editEvidence = editTarget ? safeStructuredClone(editTarget.message) : null
        return editIntent !== null || editTarget !== null
    }

    function beginEdit() {
        editDraft = message
        editMode = captureEditTarget()
        if (editMode) onEditorOpen?.()
    }

    function beginPartialEdit() {
        partialEditIntent = captureViewportEditIntent()
        partialEditTarget = partialEditIntent ? null : captureCurrentMessage()
        if (partialEditIntent || partialEditTarget) onEditorOpen?.()
    }

    function cancelPartialEdit() {
        partialEditIntent = null
        partialEditTarget = null
    }

    async function rm(e:MouseEvent, rec?:boolean){
        if (mutationBlocked()) return
        const acquired = await acquireCurrentMessage('remove-message')
        if (!acquired) return reportRowActionRefused()
        try {
            const outcome = await removeChatMessage({
                absoluteIndex: idx,
                captureTarget: () => acquired.target,
                shiftKey: e.shiftKey,
                recursive: rec ?? false,
                askRemoval: DBState.db.askRemoval ?? false,
                instantRemove: DBState.db.instantRemove ?? false,
                captureCurrent: captureCurrentChat,
                getCurrentSession: currentConversationSession,
                confirmRemoval: () => alertConfirm(language.removeChat),
                confirmInstantRemoval: () => alertCheckboxConfirm({
                    title: language.removeChat,
                    description: language.checkboxConfirmation.messageDeletionDescription,
                    checkboxLabel: language.checkboxConfirmation.laterMessageDeletion,
                    actionLabel: language.remove,
                    cancelLabel: language.cancel,
                    requireChecked: false,
                }),
                mutationBlocked,
            })
            if (outcome === 'stale') reportRowActionRefused()
        } finally {
            acquired.release()
        }
    }

    async function edit(){
        if (mutationBlocked()) return false
        const retainedIntent = editIntent
        const retainedTarget = editTarget
        const acquired = await acquireEditMessage(
            retainedIntent,
            retainedTarget,
            'edit-message',
        )
        if (!acquired) return reportRowActionRefused()
        try {
            if (mutationBlocked()) return false
            if (retainedTarget && !isEqual(resolveChatMessageTarget(retainedTarget, chatMessageContext)?.message, editEvidence)) return reportRowActionRefused()
            const result = saveCapturedChatMessage(acquired.target, chatMessageContext, editDraft)
            if (result.saved) {
                editIntent = null
                editTarget = null
                message = result.displayData
                displaya(result.displayData)
            }
            return result.saved || reportRowActionRefused()
        } finally {
            acquired.release()
        }
    }

    function startOriginalEdit() {
        if (originalEditControlDisabled) return
        beginEdit()
        if (editMode && (DBState.db.risunestChatEditPopup ?? true)) openEditPopup()
    }

    function openEditPopup() {
        editInPopup = true
        const popup: ChatEditorPopup = {
            request: {
                value: editDraft,
                save: (value) => popup.owner.save(value),
                input: (value) => popup.owner.input(value),
                cancel: () => popup.owner.cancel(),
            },
            owner: popupHandlers(),
        }
        editPopup = popup
        openTextEditorPopup(popup.request)
    }

    function popupHandlers(): ChatEditorPopupHandlers {
        return {
            save: async (value) => {
                editDraft = value
                if (!(await edit())) return false
                editMode = false
                closeEditPopup()
                return true
            },
            // Keeps the draft current for a row that is torn down while the popup is open.
            input: (value) => {
                editDraft = value
            },
            cancel: () => {
                editMode = false
                editIntent = null
                editTarget = null
                closeEditPopup()
            },
        }
    }

    function closeEditPopup() {
        editInPopup = false
        forgetEditorPopup(editPopup)
        editPopup = null
    }

    async function toggleOriginalEdit() {
        // The edit hotkey clicks this button behind the popup editor, which owns the draft.
        if (originalEditControlDisabled || editInPopup) return

        if (editMode) {
            if (await edit()) editMode = false
        } else {
            startOriginalEdit()
        }
    }

    function toggleTranslation() {
        if (translationViewControlsDisabled) return
        translated = !translated
    }

    function requestRetranslation() {
        if (translationViewControlsDisabled) return
        retranslate = true
    }

    async function handlePartialEditSave(
        detail: {
            newData: string
            sourceData: string
            target: 'original' | 'translation'
            translationKey?: string
        },
    ) {
        if (idx < 0) return false
        if (detail.target === 'translation') {
            partialEditIntent = null
            partialEditTarget = null
            if (!detail.translationKey) return false
            await updateTranslationCache(detail.translationKey, detail.newData)
            return true
        }

        if (mutationBlocked()) return false
        const retainedIntent = partialEditIntent
        const retainedTarget = partialEditTarget
        const acquired = await acquireEditMessage(retainedIntent, retainedTarget, 'partial-edit-message')
        if (!acquired) return reportRowActionRefused()
        try {
            if (mutationBlocked()) return false
            if (acquired.target.message.data !== detail.sourceData) return reportRowActionRefused()
            const result = saveCapturedChatMessage(acquired.target, chatMessageContext, detail.newData)
            if (result.saved) {
                partialEditIntent = null
                partialEditTarget = null
                message = result.displayData
                displaya(result.displayData)
            }
            return result.saved || reportRowActionRefused()
        } finally {
            acquired.release()
        }
    }

    async function updateTranslationCache(key: string, data: string) {
        await setLLMCache(key, data)
        editTranslationText = data
        chatBodyRevision += 1
    }

    async function getTranslationPartialEditContext() {
        if (!translated || DBState.db.translatorType !== 'llm') {
            return null
        }

        const key = await getTranslationCacheKey()
        if (!key) {
            return null
        }
        const data = await getLLMCache(key)
        if (data === null) {
            return null
        }

        return { key, data }
    }

    function getCbsCondition(){
        try{
            const cbsConditions:CbsConditions = {
                firstmsg: firstMessage ?? false,
                chatRole: presentedRole,
            }
            return cbsConditions
        }
        catch(e){
            return {
                firstmsg: firstMessage ?? false,
                chatRole: null,
            }
        }
    }

    async function getTranslationCacheKey(): Promise<string> {
        if (DBState.db.translateBeforeHTMLFormatting) {
            return msgDisplay
        }
        if (!DBState.db.legacyTranslation) {
            return await ParseMarkdown(
                msgDisplay,
                parserProjection ? parserChara() : character,
                'pretranslate',
                idx,
                getCbsCondition(),
                {
                    signal: parserAbortSignal,
                    ...(parserProjection
                        ? {
                              scriptContext: parserProjection.context,
                              projectedChatID: parserProjection.projectedChatID,
                          }
                        : {}),
                },
            )
        }
        return await ParseMarkdown(
            msgDisplay,
            parserProjection ? parserChara() : character,
            'notrim',
            idx,
            getCbsCondition(),
            {
                signal: parserAbortSignal,
                ...(parserProjection
                    ? {
                          scriptContext: parserProjection.context,
                          projectedChatID: parserProjection.projectedChatID,
                      }
                    : {}),
            },
        )
    }

    async function loadTranslationForEdit() {
        if (translationViewControlsDisabled) return

        loadingTranslationEdit = true
        try {
            const evidence = viewportRow?.message ?? captureCurrentMessage()?.message
            const key = await getTranslationCacheKey()
            const cached = await getLLMCache(key)
            editTranslationKey = key
            editTranslationText = cached ?? ''
            translationEvidence = evidence ? safeStructuredClone(evidence) as Message : null
            restoredTranslation = false
            editTranslationMode = true
            onEditorOpen?.()
        } catch (error) {
            editTranslationKey = null
            throw error
        } finally {
            loadingTranslationEdit = false
        }
    }

    async function saveTranslationEdit() {
        if (editTranslationKey === null) return
        if (restoredTranslation && await getTranslationCacheKey() !== editTranslationKey) {
            reportRowActionRefused()
            return
        }

        await updateTranslationCache(editTranslationKey, editTranslationText)
        editTranslationKey = null
        editTranslationMode = false
        restoredTranslation = false
        translationEvidence = null
    }

    function displaya(message:string){
        const prepared = captureContext?.preparedDisplayText?.[idx]
        if (prepared?.source === message) {
            msgDisplay = prepared.text
            return
        }
        const reads = new DisplayReadRecord()
        msgDisplay = recordChatVariableReads(reads, () => risuChatParser(message, {
            chara: name,
            chatID: idx,
            rmVar: true,
            visualize: true,
            cbsConditions: getCbsCondition(),
            ...parserArgs(),
        }))
        displayRow?.track('message', reads)
    }

    const setStatusMessage = (message:string, timeout:number = 0)=>{
        statusMessage = message
        if(timeout === 0) return
        setTimeout(() => {
            statusMessage = ''
        }, timeout)
    }


    let blankMessage = $derived((message === '{{none}}' || message === '{{blank}}' || message === '') && idx === -1 || isComment)
    let messageButtons = $derived.by(() => {
        if (idx < 0 || blankMessage) return []
        const character = DBState.db.characters[selIdState.selId]
        if (character?.chats[character.chatPage]?.isStreaming && idx === totalLength - 1) return []
        return messageButtonsForRole(presentedRole)
    })

    function messageButtonTarget() {
        const character = DBState.db.characters[selIdState.selId]
        const conversation = character?.chats[character.chatPage]
        return createMessageButtonTarget({
            character,
            conversationId: conversation?.id,
            messageIndex: idx,
            message: viewportRow?.message ?? conversation?.message?.[idx],
        })
    }
    let displayMessage = $derived(isOptimizedStreamingMessage ? rawStreamingText : message)
    let streamingThoughtMode = $derived(
        !captureContext && isOptimizedStreamingMessage
            ? (DBState.db.streamingThoughtMode ?? 'recent')
            : 'off',
    )
    let renderRawStreaming = $derived(
        !captureContext &&
            isOptimizedStreamingMessage &&
            (DBState.db.streamingDeferDisplayProcessing ?? false),
    )
    let thoughtPreview = $derived(
        renderRawStreaming && streamingThoughtMode !== 'off'
            ? getStreamingThoughtPreview(rawStreamingText)
            : null,
    )

    export function hasStreamingPreview(): boolean {
        return renderRawStreaming || streamingThoughtMode !== 'off'
    }

    function updateDisplayedMessage(){
        if(renderRawStreaming || thoughtPreview){
            return
        }
        displaya(displayMessage)
    }

    $effect.pre(() => {
        void displayRequest
        updateDisplayedMessage()
    });

    const unsubscribers:Unsubscriber[] = []

    onMount(()=>{
        if (!captureContext && !viewportRow) {
            unsubscribers.push(ReloadGUIPointer.subscribe(() => {
                updateDisplayedMessage()
            }))
            // Rows outside the chat list, such as the greeting, refresh their message text here.
            let shownReloadPointer: number | undefined
            unsubscribers.push(ReloadChatPointer.subscribe((pointers) => {
                const pointer = pointers[idx] ?? 0
                if (shownReloadPointer !== undefined && pointer !== shownReloadPointer) {
                    updateDisplayedMessage()
                }
                shownReloadPointer = pointer
            }))
        }
    })

    onDestroy(()=>{
        unsubscribers.forEach(u => u())
        displayRow?.dispose()
    })

    const initialEditor = untrack(() => restoredEditor)
    if (initialEditor) restoreEditor(initialEditor)

    // The inline editor takes the message body's place, so closing it renders the body again.
    let bodyReplacedByEditor = false
    $effect(() => {
        if (inlineEditMode) {
            bodyReplacedByEditor = true
            return
        }
        if (!bodyReplacedByEditor) return
        bodyReplacedByEditor = false
        untrack(() => onBodyRendered?.())
    })

    function RenderGUIHtml(html:string){
        try {
            const parser = new DOMParser()
            const doc = parser.parseFromString(risuChatParser(html ?? '', {
                cbsConditions: getCbsCondition(),
                ...parserArgs(),
            }), 'text/html')
            return doc.body   
        } catch (error) {
            if (error instanceof CbsHistoryAccessError) throw error
            const placeholder = document.createElement('div')
            return placeholder
        }
    }

    async function handleButtonTriggerWithin(event: UIEvent) {
        const target = event.target as HTMLElement
        const origin = target.closest('[risu-trigger], [risu-btn]')
        if (!origin) {
            return
        }

        const triggerName = origin.getAttribute('risu-trigger')
        const triggerId = origin.getAttribute('risu-id')
        const btnEvent = origin.getAttribute('risu-btn')

        await runMessageTrigger(async (currentChar, currentChat) => triggerName
            ? runTrigger(currentChar, 'manual', {
                chat: currentChat,
                manualName: triggerName,
                triggerId: triggerId || undefined,
            })
            : btnEvent
                ? runLuaInteractionTrigger('button', currentChar, btnEvent)
                : null)

        if(triggerName && triggerId) {
            setTimeout(() => {
                CurrentTriggerIdStore.set(null)
            }, 100) // Small delay to allow display mode to complete
        }
    }

    async function handleFormTriggerWithin(event: SubmitEvent) {
        // Regardless of attribute presence, prevent undesirable site exits via form submission
        event.preventDefault()

        const target = event.target as HTMLFormElement
        const formEvent = target.getAttribute('risu-form')
        if (!formEvent) {
            return
        }

        const data: Record<string, FormDataEntryValue | FormDataEntryValue[]> = Object.create(null)
        for (const [key, value] of new FormData(target, event.submitter)) {
            if (Object.hasOwn(data, key)) {
                const existing = data[key]
                if (Array.isArray(existing)) {
                    existing.push(value)
                } else {
                    data[key] = [existing, value]
                }
            } else {
                data[key] = value
            }
        }

        await runMessageTrigger((currentChar) =>
            runLuaInteractionTrigger('form', currentChar, formEvent, { data }))
    }

    async function runMessageTrigger(
        runManualTrigger: (
            currentChar: CharacterRecord,
            currentChat: ReturnType<typeof getCurrentChat>,
        ) => Promise<{ chat: ReturnType<typeof getCurrentChat> } | null | undefined>,
    ) {
        await holdScriptRun(async () => {
            if (selectedConversationOperations) {
                try {
                    await selectedConversationOperations.withCompleteSelectedConversation(
                        'manual-chat-trigger',
                        async (context) => {
                            const authority = context.requireCurrent()

                            const triggerResult = await runManualTrigger(
                                authority.character,
                                authority.conversation,
                            )
                            context.requireCurrent()
                            if (triggerResult) setCurrentChat(triggerResult.chat)
                            if (triggerResult) rebuildScriptDisplayRow(idx)
                        },
                    )
                } catch (error) {
                    if (!(error instanceof SelectedConversationPromotionStaleError)) throw error
                }
            } else {
                const currentChar = getCurrentCharacter()
                if(!currentChar) return
                const triggerResult = await runManualTrigger(currentChar, getCurrentChat())
                if(triggerResult) {
                    setCurrentChat(triggerResult.chat)
                    rebuildScriptDisplayRow(idx)
                }
            }
        })
    }

    let isBookmarked = $derived(
        captureContext
            ? captureChat?.bookmarks?.includes(captureMessage?.chatId ?? '') ?? false
            : viewportRow
                ? bookmarked ?? false
                : bookmarked ?? (
                    DBState.db.characters[selIdState.selId]
                        ?.chats[DBState.db.characters[selIdState.selId].chatPage]
                        ?.bookmarks?.includes(
                            DBState.db.characters[selIdState.selId]
                                .chats[DBState.db.characters[selIdState.selId].chatPage]
                                .message[idx]?.chatId,
                        ) ?? false
                ),
    );

    let staticCaptureSettled = false
    $effect(() => {
        const customWithoutTextBox = captureTheme === 'customHTML' &&
            !(captureSettings?.guiHTML ?? DBState.db.guiHTML)?.toLocaleLowerCase().includes('<risutextbox')
        if (!blankMessage && !inlineEditMode && !customWithoutTextBox) return
        const settled = onDisplaySettled
        void tick().then(() => settled?.())
    })
    $effect(() => {
        const customWithoutTextBox = captureTheme === 'customHTML' &&
            !captureSettings?.guiHTML?.toLocaleLowerCase().includes('<risutextbox')
        if (!captureContext || staticCaptureSettled || (!blankMessage && !customWithoutTextBox)) return
        staticCaptureSettled = true
        tick().then(() => onCaptureSettled?.(0))
    })

    async function toggleBookmark(target: CapturedChatMessageTarget) {
        await reportFailedBookmarkOperation(
            () => toggleCapturedBookmark(target, chatMessageContext, {
                mutationBlocked,
                requestName: (currentName) => alertInput(
                    language.bookmarkAskNameOrDefault,
                    [],
                    currentName,
                ),
                createMessageId: uuidv4,
                defaultName: (targetMessage) => {
                    const msgSender = targetMessage.role === 'user' ? getUserName() : name
                    const blacklist = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '+', '-', '=', '[', ']', '{', '}', '|', ';', ':', '"', "'", ',', '.', '<', '>', '/', '?']
                    let lines = targetMessage.data.split('\n')
                    lines = lines.splice(Math.floor(lines.length * 0.5))
                    const defaultLine = lines.find((line) =>
                        line && !blacklist.some((character) => line.startsWith(character)),
                    )
                    const defaultName = defaultLine
                        ? defaultLine.trim().slice(0, 50) + '...'
                        : targetMessage.data.slice(0, 50) + '...'
                    return msgSender + '| ' + defaultName
                },
            }),
            () => alertError(language.bookmarkActionFailed),
        )
    }
</script>


{#snippet genInfo()}
    <div class="flex flex-col items-end">
        {#if messageGenerationInfo && (
            captureSettings
                ? captureSettings.requestInfoInsideChat || captureSettings.aiLawApplies
                : DBState.db.requestInfoInsideChat || aiLawApplies()
        )}
            <button class="text-sm p-1 text-textcolor2 border-darkborderc float-end mr-2 my-1
                    hover:ring-darkbutton hover:ring-3 rounded-md hover:text-textcolor transition-all flex justify-center items-center" 
                    onclick={() => {
                        const currentMessage = captureContext
                            ? captureMessage
                            : viewportRow
                                ? viewportRow.message
                                : idx >= 0
                                    ? DBState.db.characters[$selectedCharID]
                                        ?.chats[DBState.db.characters[$selectedCharID].chatPage]
                                        ?.message[idx]
                                    : undefined
                        const currentGenerationInfo = currentMessage?.generationInfo
                            ?? messageGenerationInfo
                        if (!currentMessage || !currentGenerationInfo) return

                        alertRequestData({
                            genInfo: currentGenerationInfo,
                            idx: idx,
                            message: safeStructuredClone(currentMessage) as Message,
                        })
                    }}
            >
                <BotIcon size={20} />
                <span class="ml-1">
                    {capitalize(getModelInfo(messageGenerationInfo.model).shortName)}
                </span>
            </button>
        {/if}
        {#if (captureSettings?.translatorType ?? DBState.db.translatorType) === 'llm' && translated}
            <button class={"text-sm p-1 text-textcolor2 border-darkborderc float-end mr-2 my-1 rounded-md transition-all flex justify-center items-center " + (translationViewControlsDisabled ? 'opacity-50 cursor-not-allowed' : 'hover:ring-darkbutton hover:ring-3 hover:text-textcolor')}
                    disabled={translationViewControlsDisabled}
                    onclick={requestRetranslation}
            >
                <RefreshCcwIcon size={20} />
                <span class="ml-1">
                    {language.retranslate}
                </span>
            </button>
            <button class={"text-sm p-1 border-darkborderc float-end mr-2 my-1 rounded-md transition-all flex justify-center items-center " + (editTranslationMode ? 'text-blue-400 hover:ring-darkbutton hover:ring-3 hover:text-textcolor' : translationEditControlDisabled ? 'text-textcolor2 opacity-50 cursor-not-allowed' : 'text-textcolor2 hover:ring-darkbutton hover:ring-3 hover:text-textcolor')}
                    disabled={translationEditControlDisabled}
                    onclick={() => {
                        if(editTranslationMode){
                            saveTranslationEdit()
                        } else {
                            loadTranslationForEdit()
                        }
                    }}
            >
                <PencilIcon size={20} />
                <span class="ml-1">
                    {editTranslationMode ? language.editTranslationSave : language.editTranslation}
                </span>
            </button>
        {/if}
    </div>
{/snippet}

{#snippet textBox()}
    {#if editTranslationMode}
        <AutoresizeArea bind:value={editTranslationText} handleLongPress={() => {
            saveTranslationEdit()
        }} />
    {/if}
    {#if inlineEditMode}
        <AutoresizeArea bind:value={editDraft} handleLongPress={toggleOriginalEdit} />
    {:else if isComment}
        <div class="w-full flex justify-center text-textcolor2 italic mb-12">

            {#if msgDisplay.startsWith('{{specialcomment')}
                {@const parts = msgDisplay.split('::')}
                {@const type = parts[1]}

                {#if type === 'branchedfrom'}
                    <button class="text-blue-500 hover:underline"
                        onclick={async () => {
                            console.log(parts)
                            if(await changeChatTo(parts[2] ?? '')) await foldChatToMessage(parts[4])
                        }}
                    >
                        <GitBranch size={20} class="inline-block mr-1" />
                        {language.branchedText.replace("{}", parts[3] ?? '')}
                    </button>
                {/if}
            {:else}
                {msgDisplay}
            {/if}
        </div>
    {:else if blankMessage}
        <div class="w-full flex justify-center text-textcolor2 italic mb-12">
            {language.noMessage}
        </div>
    {:else}
        {@const chatReloadPointer = captureContext || viewportRow ? 0 : $ReloadGUIPointer + ($ReloadChatPointer[idx] ?? 0)}
        {@const totalLengthPointer = (idx > totalLength - 6) ? totalLength : 0}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <span class="text chat-width chattext prose minw-0"
            class:hidden={editTranslationMode}
            class:prose-invert={captureSettings?.proseInvert ?? $ColorSchemeTypeStore}
            bind:this={bodyRoot}
            onclick={() => {
            if(!captureContext && !originalEditControlDisabled && DBState.db.clickToEdit && idx > -1 && !isOptimizedStreamingMessage){
                beginEdit()
            }
        }}
            style:font-size="{0.875 * (captureZoomSize / 100)}rem"
            style:line-height="{captureLineHeight * (captureZoomSize / 100)}rem"
        >
                <ChatBody
                    reloadRevision={`${totalLengthPointer}|${chatReloadPointer}`}
                    {character}
                    {firstMessage}
                    {idx}
                    {msgDisplay}
                    {name}
                    {bodyRoot}
                    renderRevision={chatBodyRevision}
                    modelShortName={
                        messageGenerationInfo ? getModelInfo(messageGenerationInfo?.model).shortName : ''
                    }
                    role={role ?? null}
                    bind:translated={translated}
                    bind:translating={translating}
                    bind:retranslate={retranslate}
                    {renderRawStreaming}
                    {thoughtPreview}
                    {streamingThoughtMode}
                    deferStreamingDisplay={renderRawStreaming}
                    {rawStreamingText}
                    {onCaptureSettled}
                    {onCaptureError}
                    {captureContext}
                    {captureParserIndex}
                    {parserAbortSignal}
                    {onDisplaySettled}
                    {displayRequestOrder}
                    displayRow={displayRow ?? undefined}
                    {parserProjection} />
        </span>
        {#if !captureContext && idx >= 0 && !editMode && !editTranslationMode && !isOptimizedStreamingMessage && partialEditEnabled && (DBState.db.enableBlockPartialEdit || DBState.db.enableDragPartialEdit)}
            <PartialEditController
                bind:this={partialEditController}
                messageData={message}
                chatIndex={idx}
                {bodyRoot}
                blockEditEnabled={DBState.db.enableBlockPartialEdit}
                dragEditEnabled={DBState.db.enableDragPartialEdit}
                translatedView={translated}
                getTranslationEditContext={getTranslationPartialEditContext}
                on:start={beginPartialEdit}
                on:cancel={cancelPartialEdit}
                onSave={handlePartialEditSave}
            />
        {/if}
    {/if}
{/snippet}

{#snippet iconButtons(options:{applyTextColors?:boolean} = {})}
    {#if captureContext}
        <div class="grow"></div>
    {:else}
    <div class="grow flex items-center justify-end" class:text-textcolor2={options?.applyTextColors !== false}>
        {#if isComment}
            <button
                class="flex items-center hover:text-blue-500 transition-colors button-icon-remove"
                onclick={async (e) => {
                    await rm(e, true)
                }}
            >
                <TrashIcon size={20} />

            </button>
        {:else}
            <span class="text-xs">{statusMessage}</span>
            <div class="flex items-center ml-2 gap-2">
                {@render translationButton()}
                {#if (innerWidth.current ?? 0) >= 640}
                    {@render majorIconButtonsBody(false)}
                    {@render pluginMessageButtons(false)}
                    {#if DBState.db.characters[selIdState.selId]}
                        <PopupButton>
                            {@render minorIconButtonsBody(true)}
                        </PopupButton>
                    {/if}
                {:else}
                    {#if DBState.db.characters[selIdState.selId]}
                        <PopupButton>
                            {@render majorIconButtonsBody(true)}
                            {@render pluginMessageButtons(true)}
                            {@render minorIconButtonsBody(true)}
                        </PopupButton>
                    {:else}
                        {@render majorIconButtonsBody(false)}
                        {@render pluginMessageButtons(false)}
                    {/if}
                {/if}
                {@render rerolls()}

            </div>
        {/if}
    </div>
    {/if}
{/snippet}


{#snippet majorIconButtonsBody(showNames:boolean)}
    {#if DBState.db.useChatCopy && !blankMessage}
    <button class="flex items-center hover:text-blue-500 transition-colors button-icon-copy" onclick={async ()=>{
        const copyText = renderRawStreaming || thoughtPreview
            ? risuChatParser(rawStreamingText, {
                chara: name,
                chatID: idx,
                rmVar: true,
                visualize: true,
                cbsConditions: getCbsCondition(),
                ...parserArgs(),
            })
            : msgDisplay
        if(window.navigator.clipboard.write){
            try {
                alertWait(language.loading)
                const root = document.querySelector(':root') as HTMLElement;

                const deferredInlays = new DeferredInlayMarkerRegistry()
                const parser = new DOMParser()
                const doc = parser.parseFromString(
                    await ParseMarkdown(
                        copyText,
                        parserProjection ? parserChara() : getCurrentCharacter(),
                        'normal',
                        idx,
                        getCbsCondition(),
                        {
                            signal: parserAbortSignal,
                            deferredInlays,
                            scriptContext: parserProjection?.context,
                            projectedChatID: parserProjection?.projectedChatID,
                        },
                    )
                , 'text/html')
                await withResolvedDeferredInlaySources(doc, deferredInlays, async () => {
                
                doc.querySelectorAll('mark').forEach((el) => {
                    const d = el.getAttribute('risu-mark')
                    if(d === 'quote1' || d === 'quote2'){
                        const newEle = document.createElement('div')
                        newEle.textContent = el.textContent
                        newEle.setAttribute('style', `background: transparent; color: ${
                            root.style.getPropertyValue('--FontColorQuote' + d.slice(-1))
                        };`)
                        el.replaceWith(newEle)
                        return
                    }
                })
                doc.querySelectorAll('p').forEach((el) => {
                    el.setAttribute('style', `color: ${root.style.getPropertyValue('--FontColorStandard')};`)
                })
                doc.querySelectorAll('em').forEach((el) => {
                    el.setAttribute('style', `font-style: italic; color: ${root.style.getPropertyValue('--FontColorItalic')};`)
                })
                doc.querySelectorAll('strong').forEach((el) => {
                    el.setAttribute('style', `font-weight: bold; color: ${root.style.getPropertyValue('--FontColorBold')};`)
                })
                doc.querySelectorAll('em strong').forEach((el) => {
                    el.setAttribute('style', `font-weight: bold; font-style: italic; color: ${root.style.getPropertyValue('--FontColorItalicBold')};`)
                })
                doc.querySelectorAll('strong em').forEach((el) => {
                    el.setAttribute('style', `font-weight: bold; font-style: italic; color: ${root.style.getPropertyValue('--FontColorItalicBold')};`)
                })
                
                const imgs = doc.querySelectorAll('img')
                for(const img of imgs){
                    img.setAttribute('alt', 'from RisuNest')
                    const url = img.getAttribute('src')
                    
                    img.setAttribute('style', `
                        max-width: 100%;
                        margin: 10px 0;
                        border-radius: 8px;
                        box-shadow: rgba(0,0,0,0.1) 0px 2px 8px;
                        display: block;
                        margin-left: auto;
                        margin-right: auto;
                    `)
                    
                    if(url && (url.startsWith('blob:') || url.startsWith('http://asset.localhost') || url.startsWith('https://asset.localhost') || url.startsWith('https://sv.risuai') || url.startsWith('data:') || url.startsWith('http') || url.startsWith('/'))){
                        try {
                            let fetchUrl = url
                            if(url.startsWith('/')) {
                                fetchUrl = window.location.origin + url
                            }
                            
                            const dataUrl = await copyImageSourceToDataUrl(fetchUrl, 0.6)
                            if(dataUrl) img.setAttribute('src', dataUrl)
                        } catch (error) {
                            console.error('Image error:', error)
                        }
                    }
                }

                for(const source of doc.querySelectorAll('source[src^="blob:"]')){
                    try {
                        const response = await fetch(source.getAttribute('src'))
                        if(response.ok){
                            const dataUrl = await response.blob().then((blob) => new Promise<string>((resolve, reject) => {
                                const reader = new FileReader()
                                reader.onload = () => resolve(reader.result as string)
                                reader.onerror = reject
                                reader.readAsDataURL(blob)
                            }))
                            source.setAttribute('src', dataUrl)
                        }
                    } catch (error) {
                        console.error('Media error:', error)
                    }
                }

                let iconDataUrl = ''
                let hasValidImage = false
                
                try {
                    const iconImage = (await getFileSrc(DBState.db.characters[selIdState.selId].image ?? '')) ?? ''
                    
                    if(iconImage && (iconImage.startsWith('http://asset.localhost') || iconImage.startsWith('https://asset.localhost') || iconImage.startsWith('https://sv.risuai') || iconImage.startsWith('data:') || iconImage.startsWith('http') || iconImage.startsWith('/'))){
                        if(iconImage.startsWith('data:')){
                            iconDataUrl = iconImage
                            hasValidImage = true
                        } else {
                            const data = await fetch(iconImage)
                            if (data.ok) {
                                const canvas = document.createElement('canvas')
                                const ctx = canvas.getContext('2d')
                                const img = new Image()
                                img.crossOrigin = 'anonymous'
                                img.src = await data.blob().then((b) => new Promise((resolve, reject) => {
                                    const reader = new FileReader()
                                    reader.onload = () => resolve(reader.result as string)
                                    reader.onerror = reject
                                    reader.readAsDataURL(b)
                                }))
                                await new Promise((resolve, reject) => {
                                    img.onload = () => {
                                        canvas.width = img.width
                                        canvas.height = img.height
                                        ctx.drawImage(img, 0, 0)
                                        iconDataUrl = canvas.toDataURL('image/jpeg', 0.9)
                                        hasValidImage = true
                                        resolve(true)
                                    }
                                    img.onerror = () => {
                                        hasValidImage = false
                                        resolve(false)
                                    }
                                })
                            }
                        }
                    }
                } catch (error) {
                    console.error('Icon error:', error)
                    hasValidImage = false
                }

                const isUserMessage = role === 'user'
                const displayName = isUserMessage ? getUserName() : name
                const modelInfo = messageGenerationInfo ? capitalize(getModelInfo(messageGenerationInfo.model).shortName) : (isUserMessage ? 'User' : 'AI')
                
                let finalIconDataUrl = iconDataUrl
                let finalHasValidImage = hasValidImage
                
                if (isUserMessage) {
                    finalHasValidImage = false
                    const userIcon = getUserIcon()
                    if (userIcon) {
                        try {
                            const userIconSrc = await getFileSrc(userIcon)
                            if (userIconSrc && (userIconSrc.startsWith('http://asset.localhost') || userIconSrc.startsWith('https://asset.localhost') || userIconSrc.startsWith('https://sv.risuai') || userIconSrc.startsWith('data:') || userIconSrc.startsWith('http') || userIconSrc.startsWith('/'))) {
                                if (userIconSrc.startsWith('data:')) {
                                    finalIconDataUrl = userIconSrc
                                    finalHasValidImage = true
                                } else {
                                    const data = await fetch(userIconSrc)
                                    if (data.ok) {
                                        const canvas = document.createElement('canvas')
                                        const ctx = canvas.getContext('2d')
                                        const img = new Image()
                                        img.crossOrigin = 'anonymous'
                                        img.src = await data.blob().then((b) => new Promise((resolve, reject) => {
                                            const reader = new FileReader()
                                            reader.onload = () => resolve(reader.result as string)
                                            reader.onerror = reject
                                            reader.readAsDataURL(b)
                                        }))
                                        await new Promise((resolve, reject) => {
                                            img.onload = () => {
                                                canvas.width = img.width
                                                canvas.height = img.height
                                                ctx.drawImage(img, 0, 0)
                                                finalIconDataUrl = canvas.toDataURL('image/jpeg', 0.9)
                                                finalHasValidImage = true
                                                resolve(true)
                                            }
                                            img.onerror = () => {
                                                finalHasValidImage = false
                                                resolve(false)
                                            }
                                        })
                                    }
                                }
                            }
                        } catch (error) {
                            console.error('User icon error:', error)
                            finalHasValidImage = false
                        }
                    }
                }
                
                const html = `<div style="font-family: 'Segoe UI', Roboto, Arial, sans-serif; color: ${root.style.getPropertyValue('--risu-theme-textcolor')}; line-height: 1.6; max-width: 600px; margin: 1rem auto; background: ${root.style.getPropertyValue('--risu-theme-bgcolor')}; border-radius: 12px; box-shadow: 0px 4px 12px rgba(0,0,0,0.15); overflow: hidden;">
<div style="padding: 20px;">
<div style="display: flex; flex-direction: column; align-items: center; margin-bottom: 1rem; text-align: center;">
    ${finalHasValidImage ? `<img style="width: 80px; height: 80px; border-radius: 50%; border: 3px solid ${root.style.getPropertyValue('--risu-theme-darkborderc')}; margin-bottom: 0.75rem; object-fit: cover;" src="${finalIconDataUrl}" alt="profile">` : ''}
    <h3 style="color: ${root.style.getPropertyValue('--risu-theme-textcolor')}; font-weight: 600; font-size: 1.5rem; margin: 0 0 0.5rem 0;">${displayName}</h3>
    ${!isUserMessage ? `<span style="display: inline-block; border-radius: 16px; font-size: 0.8rem; padding: 0.25rem 0.75rem; background: ${root.style.getPropertyValue('--risu-theme-darkbg')}; color: ${root.style.getPropertyValue('--risu-theme-textcolor')}; border: 1px solid ${root.style.getPropertyValue('--risu-theme-darkborderc')};">${modelInfo}</span>` : ''}
</div>
<div style="border-top: 1px solid ${root.style.getPropertyValue('--risu-theme-darkborderc')}; padding-top: 1rem;">
    ${doc.body.innerHTML}
</div>
<div style="text-align: center; margin-top: 1rem; padding-top: 0.75rem; border-top: 1px solid ${root.style.getPropertyValue('--risu-theme-darkborderc')};">
    <span style="font-size: 0.75rem; color: ${root.style.getPropertyValue('--risu-theme-textcolor2')}; opacity: 0.7;">From RisuNest</span>
</div>
</div>
</div>`

                await window.navigator.clipboard.write([
                    new ClipboardItem({
                        'text/plain': new Blob([copyText], {type: 'text/plain'}),
                        'text/html': new Blob([html], {type: 'text/html'})
                    })
                ])
                })
                alertNormal(language.copied)
                return
            }
            catch (e) {
                alertClear()
                window.navigator.clipboard.writeText(copyText).then(() => {
                    setStatusMessage(language.copied)
                })
            }
        }
        window.navigator.clipboard.writeText(copyText).then(() => {
            setStatusMessage(language.copied)
        })
    }}>
        <CopyIcon size={20}/>
        {#if showNames}
            <span class="ml-1">{language.copy}</span>
        {/if}
    </button>    
{/if}
{#if idx > -1}
    {#if DBState.db.characters[selIdState.selId].ttsMode !== 'none' && (DBState.db.characters[selIdState.selId].ttsMode)}
        <button class="flex items-center hover:text-blue-500 transition-colors button-icon-tts" onclick={()=>{
            return sayTTS(null, isOptimizedStreamingMessage ? rawStreamingText : message)
        }}>
            <Volume2Icon size={20}/>
            {#if showNames}
                <span class="ml-1">TTS</span>
            {/if}
        </button>
    {/if}
    <button class="flex items-center hover:text-blue-500 transition-colors button-icon-remove" onclick={(e) => rm(e, false)} use:longpress={(e) => rm(e, true)}>
            <TrashIcon size={20}/>

            {#if showNames}
                <span class="ml-1">{language.remove}</span>
            {/if}
        </button>
{/if}
{/snippet}

{#snippet pluginMessageButtons(showNames:boolean)}
    {#each messageButtons as button (button.id)}
        <button class="flex items-center hover:text-blue-500 transition-colors button-icon-plugin" title={button.name} aria-label={button.name} onclick={() => {
            const target = messageButtonTarget()
            if (target) invokeMessageButton(button, target)
        }}>
            <PluginDefinedIcon ico={button} />
            {#if showNames || button.iconType === 'none'}
                <span class="ml-1">{button.name}</span>
            {/if}
        </button>
    {/each}
{/snippet}

{#snippet translationButton(showNames = false)}
    {#if DBState.db.translator !== '' && !blankMessage && !isOptimizedStreamingMessage}
        <button
            class={"flex items-center transition-colors button-icon-translate " + (translationViewControlsDisabled ? 'opacity-50 cursor-not-allowed' : 'cursor-pointer hover:text-blue-500') + (translated ? ' text-blue-400' : '')}
            class:translating={translating}
            disabled={translationViewControlsDisabled}
            onclick={toggleTranslation}
        >
            <LanguagesIcon />
            {#if showNames}
                <span class="ml-1">{language.translate}</span>
            {/if}
        </button>
    {/if}
    {#if idx > -1 && !isOptimizedStreamingMessage}
        <button
            class={"flex items-center transition-colors button-icon-edit " + (editMode ? 'text-blue-400 hover:text-blue-500' : originalEditControlDisabled ? 'opacity-50 cursor-not-allowed' : 'hover:text-blue-500')}
            disabled={originalEditControlDisabled}
            onclick={toggleOriginalEdit}
        >
            <PencilIcon size={20}/>

            {#if showNames}
                <span class="ml-1">{language.edit}</span>
            {/if}
        </button>
    {/if}
{/snippet}

{#snippet rerolls()}
    {#if rerollIcon || altGreeting}
        <ResponseCandidateControls {currentPage} {totalPages} greeting={altGreeting} showPages={!firstMessage || DBState.db.showFirstMessagePages} dynamic={rerollIcon === 'dynamic'} busy={$doingChat || $activeRerollConversations.length > 0} previous={unReroll} next={altGreeting ? onReroll : onNextReroll} generate={onReroll} />
    {/if}
{/snippet}

{#snippet minorIconButtonsBody(showNames:boolean)}
    
    {#if DBState.db.enableBookmark}
        <button class="flex items-center hover:text-blue-500 transition-colors button-icon-bookmark {isBookmarked ? 'text-yellow-400' : ''}" onclick={async () => {
            if (mutationBlocked()) return
            const acquired = await acquireCurrentMessage('toggle-bookmark')
            if (!acquired) return reportRowActionRefused()
            try {
                await sleep(1)
                if (mutationBlocked()) return
                await toggleBookmark(acquired.target)
            } finally {
                acquired.release()
            }
        }}>
            <BookmarkIcon size={20}/>
            {#if showNames}
                <span class="ml-1">{language.bookmark}</span>
            {/if}
        </button>
    {/if}

    <button class="flex items-center hover:text-blue-500 transition-colors button-icon-branch" onclick={async () => {
        if (mutationBlocked()) return
        const acquired = await acquireCurrentMessage('create-message-branch')
        if (!acquired) return reportRowActionRefused()
        try {
            await sleep(1)
            if (mutationBlocked()) return
            const created = await createCapturedConversationBranch({
                target: acquired.target,
                context: chatMessageContext,
                runtime: getPersistentDataRuntime(),
                createFolderOnBranch: DBState.db.createFolderOnBranch === true,
                createId: uuidv4,
                createBranchName: (sourceName) => createChatCopyName(sourceName, 'Branch'),
                navigateToBranch: changeChatTo,
            })
            if (!created) reportRowActionRefused()
        } finally {
            acquired.release()
        }
    }}>
        <SplitIcon size={20}/>
        {#if showNames}
            <span class="ml-1">{language.branch}</span>
        {/if}
    </button>

    <button class="flex items-center hover:text-blue-500 transition-colors button-icon-disable" onclick={async () => {
        if (mutationBlocked()) return
        const acquired = await acquireCurrentMessage('toggle-message-disabled')
        if (!acquired) return reportRowActionRefused()
        try {
            await sleep(1)
            if (mutationBlocked()) return
            if (!toggleCapturedMessageDisabled(acquired.target, chatMessageContext, 'message')) reportRowActionRefused()
        } finally {
            acquired.release()
        }
    }}>
        <PowerOff size={20}/>
        {#if showNames}
            <span class="ml-1">{language.disableMessage}</span>
        {/if}
    </button>

    <button class="flex items-center hover:text-blue-500 transition-colors button-icon-disable-above" onclick={async () => {
        if (mutationBlocked()) return
        const acquired = await acquireCurrentMessage('toggle-messages-above-disabled')
        if (!acquired) return reportRowActionRefused()
        try {
            await sleep(1)
            if (mutationBlocked()) return
            if (!toggleCapturedMessageDisabled(acquired.target, chatMessageContext, 'allBefore')) reportRowActionRefused()
        } finally {
            acquired.release()
        }
    }}>
        <Scissors size={20}/>
        {#if showNames}
            <span class="ml-1">{language.disableAbove}</span>
        {/if}
    </button>
{/snippet}

{#snippet senderIcon(options:{rounded?:boolean,styleFix?:string} = {})}
    {#if !blankMessage && !hideCaptureIcon}
        {#if (captureCharacter?.chaId ?? DBState.db.characters[selIdState.selId]?.chaId) === "§playground"}
        <div class="shadow-lg border-textcolor2 border flex justify-center items-center text-textcolor2" style={options?.styleFix ?? `height:${captureIconSize * 3.5 / 100}rem;width:${captureIconSize * 3.5 / 100}rem;min-width:${captureIconSize * 3.5 / 100}rem`}
            class:rounded-md={options?.rounded} class:rounded-full={options?.rounded}>
                {#if name === 'assistant'}
                    <BotIcon />
                {:else}
                    <UserIcon />
                {/if}
            </div>
        {:else}
            {#await img}
                <div class="shadow-lg bg-textcolor2" style={options?.styleFix ??`height:${captureIconSize * 3.5 / 100}rem;width:${captureIconSize * 3.5 / 100}rem;min-width:${captureIconSize * 3.5 / 100}rem`}
                class:rounded-md={!options?.rounded} class:rounded-full={options?.rounded}></div>
            {:then m}
                {#if largePortrait && (!options?.rounded)}
                    <div class="shadow-lg bg-textcolor2" style={m + (options?.styleFix ?? `height:${captureIconSize * 3.5 / 100 / 0.75}rem;width:${captureIconSize * 3.5 / 100}rem;min-width:${captureIconSize * 3.5 / 100}rem`)}
                    class:rounded-md={!options?.rounded} class:rounded-full={options?.rounded}></div>
                {:else}
                    <div class="shadow-lg bg-textcolor2" style={m + (options?.styleFix ?? `height:${captureIconSize * 3.5 / 100}rem;width:${captureIconSize * 3.5 / 100}rem;min-width:${captureIconSize * 3.5 / 100}rem`)}
                    class:rounded-md={!options?.rounded} class:rounded-full={options?.rounded}></div>
                {/if}
            {/await}
        {/if}
    {/if}
{/snippet}

{#snippet renderGuiHtmlPart(dom:HTMLElement)}
    {#if dom.tagName === 'IMG'}
        <img class={dom.getAttribute('class') ?? ''} alt="" style={dom.getAttribute('style') ?? ''} />
    {:else if dom.tagName === 'A'}
        <a target="_blank" rel="noreferrer" href={
            (dom.getAttribute('href') && dom.getAttribute('href').startsWith('https')) ? dom.getAttribute('href') : ''
        } class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </a>
    {:else if dom.tagName === 'SPAN'}
        <span class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </span>
    {:else if dom.tagName === 'DIV'}
        <div class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </div>
    {:else if dom.tagName === 'P'}
        <p class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </p>
    {:else if dom.tagName === 'H1'}
        <h1 class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </h1>
    {:else if dom.tagName === 'H2'}
        <h2 class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </h2>
    {:else if dom.tagName === 'H3'}
        <h3 class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </h3>
    {:else if dom.tagName === 'H4'}
        <h4 class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </h4>
    {:else if dom.tagName === 'H5'}
        <h5 class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </h5>
    {:else if dom.tagName === 'H6'}
        <h6 class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </h6>
    {:else if dom.tagName === 'UL'}
        <ul class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </ul>
    {:else if dom.tagName === 'OL'}
        <ol class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </ol>
    {:else if dom.tagName === 'LI'}
        <li class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </li>
    {:else if dom.tagName === 'TABLE'}
        <table class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </table>
    {:else if dom.tagName === 'TR'}
        <tr class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </tr>
    {:else if dom.tagName === 'TD'}
        <td class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </td>
    {:else if dom.tagName === 'TH'}
        <th class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </th>
    {:else if dom.tagName === 'HR'}
        <hr class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''} />
    {:else if dom.tagName === 'BR'}
        <br class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''} />
    {:else if dom.tagName === 'CODE'}
        <code class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </code>
    {:else if dom.tagName === 'PRE'}
        <pre class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </pre>
    {:else if dom.tagName === 'BLOCKQUOTE'}
        <blockquote class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </blockquote>
    {:else if dom.tagName === 'EM'}
        <em class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </em>
    {:else if dom.tagName === 'STRONG'}
        <strong class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </strong>
    {:else if dom.tagName === 'U'}
        <u class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </u>
    {:else if dom.tagName === 'DEL'}
        <del class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </del>
    {:else if dom.tagName === 'BUTTON'}
        <button class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </button>
    {:else if dom.tagName === 'RISUTEXTBOX'}
        {@render textBox()}
    {:else if dom.tagName === 'RISUICON'}
        {@render senderIcon()}
    {:else if dom.tagName === 'RISUBUTTONS'}
        {@render iconButtons()}
    {:else if dom.tagName === 'RISUGENINFO'}
        {@render genInfo()}
    {:else if dom.tagName === 'STYLE'}
        <svelte:element this={'style'}>
            {dom.innerHTML}
        </svelte:element>
    {:else}
        <div class={dom.getAttribute('class') ?? ''} style={dom.getAttribute('style') ?? ''}>
            {@render renderChilds(dom)}
        </div>
    {/if}

    
{/snippet}

{#snippet renderChilds(dom:HTMLElement)}
    {#each dom.childNodes as node}
        {#if node.nodeType === Node.TEXT_NODE}
            {node.textContent}
        {:else if node.nodeType === Node.ELEMENT_NODE}
            {@render renderGuiHtmlPart((node as HTMLElement))}
        {/if}
    {/each}
{/snippet}


{#if disabled === true}
<div class="w-full border-t-2 border-dashed border-blue-500"></div>
{/if}
<div class="flex max-w-full justify-center risu-chat"
     data-chat-index={idx}
     data-chat-id={presentedChatId}
     style={isLastMemory ? `border-top:${captureSettings?.memoryLimitThickness ?? DBState.db.memoryLimitThickness}px solid rgba(98, 114, 164, 0.7);` : ''}
     onclickcapture={handleButtonTriggerWithin}
     onsubmitcapture={handleFormTriggerWithin}>
    <div class="text-textcolor mt-1 ml-4 mr-4 mb-1 p-2 bg-transparent grow border-t-gray-900 border-opacity/30 border-transparent flexium items-start max-w-full" >
        {#if captureTheme === 'mobilechat' && !blankMessage}
            <div class={role === 'user' ? "flex items-start w-full justify-end" : "flex items-start"}>
                {#if role !== 'user'}
                    {@render senderIcon({rounded: true})}
                {/if}
                <div
                    class="bg-gray-100 rounded-lg p-3 max-w-[70%] mx-2"
                    class:rounded-tl-none={role !== 'user'}
                    class:rounded-tr-none={role === 'user'}
                >
                    <p class="text-gray-800">{@render textBox()}</p>
                    {#if presentedTime}
                        <span class="text-xs text-textcolor2 mt-1 block">
                            {new Intl.DateTimeFormat(undefined, {
                                hour: '2-digit',
                                minute: '2-digit',
                                second: '2-digit',
                                month: '2-digit',
                                day: '2-digit',
                                hour12: false
                            }).format(presentedTime)}
                        </span>
                    {/if}
                </div>
                {#if role === 'user'}
                    {@render senderIcon({rounded: true})}
                {/if}
            </div>
        {:else if captureTheme === 'cardboard' && !blankMessage}
            <div class="w-full flex flex-col px-0 sm:px-4 py-4 relative">
                <div class="bg-linear-to-b from-gray-100 to-gray-200 rounded-lg shadow-lg border-gray-400 border p-4 flex flex-col">
                    <div class="flex gap-4 mt-2 flex-col sm:flex-row">
                        <div class="flex flex-col items-center">
                            <div class="sm:h-96 sm:w-72 sm:min-w-72 w-48 h-64">
                                {@render senderIcon({rounded: false, styleFix:'height:100%;width:100%;'})}
                            </div>
                            <h2 class="text-base font-bold text-gray-500 text-center mt-2 max-w-full text-ellipsis">{name}</h2>

                        </div>
                        {#if inlineEditMode}
                            <textarea class="grow h-138 sm:h-96 overflow-y-auto bg-transparent text-black p-2 mb-2 resize-none message-edit-area" bind:value={editDraft}></textarea>
                        {:else}
                            <div class="grow h-138 sm:h-96 overflow-y-auto p-2 mb-2 sm:mb-0">
                                {@render textBox()}
                            </div>
                        {/if}
                    </div>
                </div>
                <div class="absolute bottom-0 right-0 bg-linear-to-b from-gray-200 to-gray-300 p-2 rounded-md border border-gray-400 text-gray-400">
                    {@render iconButtons({applyTextColors: false})}
                </div>
            </div>
        {:else if captureTheme === 'customHTML' && !blankMessage}
            {@render renderGuiHtmlPart(presentedLayout.gui!)}
        {:else}
            {@render senderIcon({rounded: captureSettings?.roundIcons ?? DBState.db.roundIcons})}
            <span class="flex flex-col ml-4 w-full max-w-full min-w-0 text-black">
                <div class="flexium items-center chat-width">
                    {#if (captureCharacter?.chaId ?? DBState.db.characters[selIdState.selId]?.chaId) === "§playground" && !blankMessage && (
                        captureContext
                            ? captureMessage
                            : viewportRow
                                ? viewportRow.message
                                : DBState.db.characters[selIdState.selId]
                                    ?.chats?.[DBState.db.characters[selIdState.selId]?.chatPage]
                                    ?.message?.[idx]
                    )}
                        <span class="chat-width text-xl border-darkborderc flex items-center text-textcolor">
                            <span>{presentedRole === 'char' ? 'Assistant' : 'User'}</span>
                            <button class="ml-2 text-textcolor2 hover:text-textcolor button-icon-toggle-role" onclick={async () => {
                                if (mutationBlocked()) return
                                const acquired = await acquireCurrentMessage('toggle-message-role')
                                if (!acquired) return reportRowActionRefused()
                                try {
                                    if (mutationBlocked()) return
                                    if (!toggleCapturedMessageRole(acquired.target, chatMessageContext)) return reportRowActionRefused()
                                    ReloadChatPointer.update((v) => {
                                        v[idx] = (v[idx] ?? 0) + 1
                                        return v
                                    })
                                } finally {
                                    acquired.release()
                                }
                            }}><ArrowLeftRightIcon size="18" /></button>
                        </span>
                    {:else if !blankMessage && !hideCaptureIcon}
                        <div class="chat-width text-xl unmargin text-textcolor flex items-center">
                            <span>{name}</span>
                        </div>
                    {/if}
                    {@render iconButtons()}
                </div>
                {@render genInfo()}
                {@render textBox()}
            </span>
        {/if}
    </div>
</div>

{#if disabled}
<div class={{
    "w-full border-t-2 border-dashed": true,
    "border-blue-500": disabled === true,
    "border-amber-500": disabled === 'allBefore',
}}></div>
{/if}
