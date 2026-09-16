<script lang="ts">
    import { v4 } from 'uuid'
    import { alertConfirm } from 'src/ts/alert'

    import Suggestion from './Suggestion.svelte';
    import { createLiveChatParserIndirections, createLiveChatParserSource } from 'src/ts/liveDisplayParserLease';
    import { CameraIcon, DatabaseIcon, DicesIcon, GlobeIcon, ImagePlusIcon, LanguagesIcon, Laugh, MenuIcon, MicOffIcon, PackageIcon, Plus, RefreshCcwIcon, ReplyIcon, Send, StepForwardIcon, XIcon, BrainIcon, ArrowDown, SparkleIcon } from "@lucide/svelte";
    import { selectedCharID, PlaygroundStore, createSimpleCharacter, hypaV3ModalOpen, ScrollToMessageStore, additionalChatMenu, additionalFloatingActionButtons, easyPanelStore, chatPanelStore } from "../../ts/stores.svelte";
    import { onDestroy } from 'svelte';
    import { type Chat as ChatRecord, type Database, type character, type groupChat, type Message } from "../../ts/storage/database.svelte";
    import { DBState } from 'src/ts/stores.svelte';
    import { chatProcessStage, doingChat, sendChat, notifyGenerationCompletion } from "../../ts/process/index.svelte";
    import { getPersonaPrompt, parseKeyValue, sleep } from "../../ts/util";
    import { language } from "../../lang";
    import { isExpTranslator, translate } from "../../ts/translator/translator";
    import { alertError, alertNormal, showHypaV2Alert } from "../../ts/alert";
    import sendSound from '../../etc/send.mp3'
    import { processScript, type ProcessScriptCaptureContext } from "src/ts/process/scripts";
    import { stopTTS } from "src/ts/process/tts";
    import MainMenu from '../UI/MainMenu.svelte';
    import AssetInput from './AssetInput.svelte';
    import { aiLawApplies, chatFoldedState, chatFoldedStateMessageIndex, downloadFile, LocalWriter } from 'src/ts/globalApi.svelte';
    import { runTrigger } from 'src/ts/process/triggers';
    import { generateResponseCandidate, moveResponseCandidate } from 'src/ts/durableReroll'
    import { responseRange } from 'src/ts/responseVariants'
    import { processMultiCommand } from 'src/ts/process/command'
    import { postChatFile } from 'src/ts/process/files/multisend';
    import InlayFilePreview from './InlayFilePreview.svelte';
    import { ConnectionOpenStore } from 'src/ts/sync/multiuser';
    import Chats from './Chats.svelte';
    import Button from '../UI/GUI/Button.svelte';
    import PluginDefinedIcon from '../Others/PluginDefinedIcon.svelte';
    import { getActiveConversationSession, getPersistentDataRuntime } from '../../ts/storage/persistentDataRuntime.svelte';
    import type { ActiveConversationSession } from '../../ts/storage/activeConversationSession';
    import {
        appendConversationMessage,
        captureConversationMutationTarget,
        isConversationMutationTargetCurrent,
        type ConversationMutationTarget,
    } from '../../ts/conversationMutations';
    import { appendDefaultChatInput } from './defaultChatInput';

    import {
        LatestChatScrollRequestGuard,
        navigateCapturedChatMessage,
        type CapturedChatMessageTarget,
    } from '../../ts/chatMessageUi'
    import type { ChatViewportHandle } from '../../ts/chatViewport';

    import ChatScreenshotDialog from './ChatScreenshotDialog.svelte'
    import ChatScreenshotCaptureSurface from './ChatScreenshotCaptureSurface.svelte';
    import {
        snapshotChatScreenshotCharacter,
        type ChatScreenshotDialogSnapshot,
        type ChatScreenshotRenderContext,
    } from 'src/ts/chatScreenshotRange';
    import {
        openChatScreenshotSourceLease,
        type ChatScreenshotSourceLease,
    } from 'src/ts/chatScreenshotSourceLease';
    import { canExportLongScreenshotArchive, captureChatScreenshot, createDomScreenshotEncoder, type ChatScreenshotSurface } from 'src/ts/chatScreenshotCapture';
    import { createStreamingScreenshotArchive } from 'src/ts/chatScreenshotArchive';
    import { createAndroidScreenshotArchiveWriter, createNativeScreenshotArchiveWriter, describeScreenshotPublicationError } from 'src/ts/nativeScreenshotArchiveWriter';
    import { isTauri, isTauriAndroid, isTauriDesktop } from 'src/ts/platform';
    import { isAndroidSafFileJobsEnabled } from 'src/ts/storage/androidSafBridge';
    import { getModuleAssets, getModuleLorebooks, getModuleRegexScripts, getModules, getModuleTriggers } from 'src/ts/process/modules';
    import { ColorSchemeTypeStore } from 'src/ts/gui/colorscheme';
    import { HideIconStore } from 'src/ts/stores.svelte';
    import { SelectedConversationViewportBinding } from '../../ts/selectedConversationViewportBinding';
    import { isMetadataOnlySelectedConversation } from '../../ts/storage/selectedConversationLifecycle';
    import { pluginV2 } from '../../ts/plugins/plugins.svelte';
    import {
        bindCompleteLiveParserContextAuthority,
        collectLiveChatParserUnsafeDependencies,
        createSelectedConversationLiveParserProjectionResolver,
    } from '../../ts/selectedConversationLiveParserProjection';
    import type { CurrentChatMessageTarget } from '../../ts/chatMessageUi';
    import {
        createSelectedConversationOperations,
        type CompleteSelectedConversationContext,
    } from '../../ts/selectedConversationOperations';
    import { SelectedConversationPromotionStaleError } from '../../ts/storage/activeWorkingSet.svelte';

    interface ConversationOperationAuthority {
        character: Database['characters'][number]
        conversation: ChatRecord
        session: ActiveConversationSession | null
    }

    interface ConversationOperationContext {
        requireCurrent(): ConversationOperationAuthority
    }
    import { readSelectedConversationLatestTail } from '../../ts/selectedConversationTail';
    import { writeConversationSuggestions } from '../../ts/autoSuggestionMetadata';
    import { moveAlternateGreeting } from './conversationStartMutations';

    const loadPlaygroundMenu = () => import('../Playground/PlaygroundMenu.svelte').then(m => m.default);
    
    interface Props {
        openModuleList?: boolean;
        openChatList?: boolean;
        customStyle?: string;
    }

    let messageInput:string = $state('')
    let messageInputTranslate:string = $state('')
    let openMenu = $state(false)
    let autoMode = $state(false)
    let rerollBusy = $state(false)
    let doingChatInputTranslate = false
    let toggleStickers: boolean = $state(false)
    let fileInput: string[] = $state([])
    let showNewMessageButton = $state(false)
    let chatsInstance: ChatViewportHandle | undefined = $state()
    let isScrollingToMessage = $state(false)
    let { openModuleList = $bindable(false), openChatList = $bindable(false), customStyle = '' }: Props = $props();
    const persistentRuntime = getPersistentDataRuntime()
    const selectedConversationOperations = createSelectedConversationOperations({
        captureSelectedConversationTarget: () =>
            persistentRuntime.captureSelectedConversationTarget(),
        acquireCompleteConversation: (reason, target) =>
            persistentRuntime.acquireCompleteConversation(reason, target),
        captureCurrent: () => {
            const character = DBState.db.characters[$selectedCharID]
            const conversation = character?.chats[character.chatPage]
            return character && conversation ? { character, conversation } : null
        },
        getCurrentSession: () => persistentRuntime.getActiveConversationSession(),
        getCurrentViewportSource: () =>
            persistentRuntime.getActiveConversationViewportSource(),
    })
    let viewportBindingRevision = $state(0)
    const selectedConversationViewport = new SelectedConversationViewportBinding(
        persistentRuntime,
        () => viewportBindingRevision += 1,
    )
    let currentCharacter = $derived(DBState.db.characters[$selectedCharID])
    let conversationViewportSource = $derived.by(() => {
        void viewportBindingRevision
        return selectedConversationViewport.source
    })
    let conversationViewportNavigationGeneration = $derived.by(() => {
        void viewportBindingRevision
        return persistentRuntime.getNavigationGeneration()
    })
    let currentChat = $derived.by(() => {
        void viewportBindingRevision
        if (conversationViewportSource) return undefined
        const conversation = currentCharacter?.chats[currentCharacter.chatPage]
        if (!conversation || isMetadataOnlySelectedConversation(conversation)) return undefined
        return conversation.message ?? []
    })
    let currentMessageCount = $derived.by(() => {
        void viewportBindingRevision
        return conversationViewportSource
            ? selectedConversationViewport.totalMessages
            : currentChat?.length ?? 0
    })
    let tailCurrentMessage = $derived.by(() => {
        void viewportBindingRevision
        return conversationViewportSource
            ? selectedConversationViewport.tailMessage
            : currentChat?.at(-1)
    })
    let canContinueResponse = $derived(
        currentMessageCount >= 2 && tailCurrentMessage?.role === 'char',
    )
    const scrollRequestGuard = new LatestChatScrollRequestGuard()

    function requireConversationMutationTarget(
        context: ConversationOperationContext,
    ): ConversationMutationTarget {
        const authority = context.requireCurrent()
        return captureConversationMutationTarget(
            authority.character,
            authority.conversation,
            authority.session,
        )
    }

    async function runSelectedConversationOperation<T>(
        reason: string,
        operation: (context: ConversationOperationContext) => T | Promise<T>,
    ): Promise<T | null> {
        try {
            if (persistentRuntime.captureSelectedConversationTarget()) {
                return await selectedConversationOperations.withCompleteSelectedConversation(
                    reason,
                    operation as (context: CompleteSelectedConversationContext) => T | Promise<T>,
                )
            }
            const character = DBState.db.characters[$selectedCharID]
            const conversation = character?.chats[character.chatPage]
            if (!character || !conversation || isMetadataOnlySelectedConversation(conversation)) {
                return null
            }
            const session = persistentRuntime.getActiveConversationSession()
            const context: ConversationOperationContext = {
                requireCurrent() {
                    const currentCharacter = DBState.db.characters[$selectedCharID]
                    const currentConversation = currentCharacter?.chats[currentCharacter.chatPage]
                    if (
                        currentCharacter !== character ||
                        currentConversation !== conversation ||
                        persistentRuntime.getActiveConversationSession() !== session
                    ) throw new SelectedConversationPromotionStaleError()
                    return { character, conversation, session }
                },
            }
            context.requireCurrent()
            return await operation(context)
        } catch (error) {
            if (error instanceof SelectedConversationPromotionStaleError) return null
            throw error
        }
    }

    function conversationTargetIsCurrent(target: ConversationMutationTarget): boolean {
        const character = DBState.db.characters[$selectedCharID]
        return isConversationMutationTargetCurrent(
            target,
            character,
            character?.chats[character.chatPage],
            getActiveConversationSession(),
        )
    }

    let screenshotDialogOpen = $state(false)
    let screenshotTotalTurns = $state(0)
    let screenshotRunning = $state(false)
    let screenshotCompletedTurns = $state(0)
    let screenshotError = $state('')
    let screenshotController: AbortController | null = null
    let screenshotSurface: ChatScreenshotSurface | undefined
    let screenshotDialogSnapshot: ChatScreenshotDialogSnapshot | null = null
    let screenshotSourceLease: ChatScreenshotSourceLease | null = null
    let screenshotOpenGeneration = 0
    let screenshotOpening = false

    function scrollToBottom() {
        chatsInstance?.scrollToLatestMessage();
    }
    const scrollTargetContext = {
        captureCurrent: () => {
            const character = DBState.db.characters[$selectedCharID]
            const conversation = character?.chats[character.chatPage]
            return character && conversation ? { character, conversation } : null
        },
        getCurrentSession: getActiveConversationSession,
    }
    $effect(() => {
        if($ScrollToMessageStore && chatsInstance){
            const target = $ScrollToMessageStore
            ScrollToMessageStore.set(null)
            void scrollToMessage(target, scrollRequestGuard.begin())
        }
    })

    async function scrollToMessage(
        target: CapturedChatMessageTarget,
        requestGeneration: number,
    ){
        isScrollingToMessage = true
        try {
            const viewport = chatsInstance
            if (!viewport) return
            await navigateCapturedChatMessage({
                target,
                context: scrollTargetContext,
                guard: scrollRequestGuard,
                requestGeneration,
                viewport,
            })
        } finally {
            if (scrollRequestGuard.isCurrent(requestGeneration)) {
                isScrollingToMessage = false
            }
        }
    }

    let previousFoldIndex = -1
    $effect(() => {
        const index = chatFoldedStateMessageIndex.index
        const viewport = chatsInstance
        if (index < 0) {
            previousFoldIndex = -1
            return
        }
        if (!viewport || index === previousFoldIndex) return
        previousFoldIndex = index
        void viewport.jumpTo(index, { align: 'center', highlight: true })
    })

    async function send(){
        return sendMain(false)
    }
    async function sendContinue(){
        return sendMain(true)
    }

    async function sendMain(continueResponse:boolean) {
        if($doingChat){
            return
        }
        return runSelectedConversationOperation(
            continueResponse ? 'continue-response' : 'send-message',
            (context) => sendMainComplete(context, continueResponse),
        )
    }

    async function sendMainComplete(
        context: ConversationOperationContext,
        continueResponse: boolean,
    ) {
        let mutationTarget = requireConversationMutationTarget(context)
        const character = mutationTarget.character
        let messages = mutationTarget.conversation.message

        if(messageInput.startsWith('/')){
            const commandProcessed = await processMultiCommand(messageInput)
            context.requireCurrent()
            if(commandProcessed !== false){
                messageInput = ''
                return
            }
            mutationTarget = requireConversationMutationTarget(context)
            messages = mutationTarget.conversation.message
        }

        if(fileInput.length > 0){
            for(const file of fileInput){
                messageInput += `{{inlayed::${file}}}`
            }
            fileInput = []
        }

        if(messageInput === ''){
            if(character.type !== 'group'){
                if(messages.length === 0 || messages[messages.length - 1].role !== 'user'){
                    if(DBState.db.useSayNothing){
                        appendConversationMessage(mutationTarget, {
                            role: 'user',
                            data: '*says nothing*',
                            name: $ConnectionOpenStore ? DBState.db.username : null
                        })
                    }
                }
            }
        }
        else{
            if(character.type === 'character'){
                const appended = await appendDefaultChatInput({
                    target: mutationTarget,
                    recaptureTarget: () =>
                        requireConversationMutationTarget(context),
                    runInputTrigger: (onConversationCommit) =>
                        runTrigger(character, 'input', {
                            chat: mutationTarget.conversation,
                            onConversationCommit,
                        }),
                    processInput: (onConversationCommit) =>
                        processScript(
                            character,
                            messageInput,
                            'editinput',
                            {},
                            { onConversationCommit },
                        ),
                    isTargetCurrent: (target) => {
                        context.requireCurrent()
                        return conversationTargetIsCurrent(target)
                    },
                    createMessage: (data) => ({
                        role: 'user',
                        data,
                        time: Date.now(),
                        name: $ConnectionOpenStore ? DBState.db.username : null,
                    }),
                })
                context.requireCurrent()
                if (!appended) return
                mutationTarget = requireConversationMutationTarget(context)
            }
            else{
                appendConversationMessage(mutationTarget, {
                    role: 'user',
                    data: messageInput,
                    time: Date.now(),
                    name: $ConnectionOpenStore ? DBState.db.username : null
                })
            }
        }
        messageInput = ''
        messageInputTranslate = ''
        mutationTarget = requireConversationMutationTarget(context)
        await sleep(10)
        context.requireCurrent()
        mutationTarget = requireConversationMutationTarget(context)
        updateInputSizeAll()
        await sendChatMainComplete(context, continueResponse)
        context.requireCurrent()

    }

    async function reroll() {
        if ($doingChat || rerollBusy) return
        rerollBusy = true
        abortController = new AbortController()
        try {
            await runSelectedConversationOperation('reroll-response', async (context) => {
                const { character, conversation } = context.requireCurrent()
                const navigation = persistentRuntime.getNavigationGeneration()
                const owner = () => DBState.db.characters.find((item) => item.chaId === character.chaId)
                const current = () => owner()?.chats.find((chat) => chat.id === conversation.id)
                const isCurrent = () => {
                    const selected = DBState.db.characters[$selectedCharID]
                    return navigation === persistentRuntime.getNavigationGeneration() &&
                        selected?.chaId === character.chaId && selected.chats[selected.chatPage]?.id === conversation.id
                }
                const completed = await generateResponseCandidate({
                    chat: conversation,
                    currentChat: current,
                    session: () => {
                        const session = getActiveConversationSession()
                        return session?.matchesConversation(character.chaId, current()) ? session : null
                    },
                    isCurrent,
                    createId: v4,
                    flush: () => persistentRuntime.flushPendingData('reroll-candidate'),
                    generate: () => sendChat(-1, { signal: abortController!.signal }),
                    aborted: () => abortController!.signal.aborted,
                })
                if (completed) {
                    await persistentRuntime.acknowledgeGenerationCompletion()
                    await notifyGenerationCompletion(current()?.message.at(-1)?.data ?? '')
                    if (DBState.db.playMessage) new Audio(sendSound).play().catch(() => {})
                }
            })
        } catch (error) {
            alertError(error)
        } finally {
            rerollBusy = false
            $doingChat = false
        }
    }

    async function nextReroll() {
        if ($doingChat || rerollBusy) return
        let generate = false
        await runSelectedConversationOperation('next-response-candidate', async (context) => {
            const { conversation, session } = context.requireCurrent()
            if (moveResponseCandidate(conversation, session, 1, v4)) {
                await persistentRuntime.flushPendingData('select-response-candidate')
                return
            }
            if (!responseRange(conversation.message)) return
            const version = session?.version
            const selected = JSON.stringify(conversation.message.at(-1))
            const confirmed = await alertConfirm(language.confirmNewResponseCandidate)
            context.requireCurrent()
            generate =
                confirmed &&
                session?.version === version &&
                JSON.stringify(conversation.message.at(-1)) === selected
        })
        if (generate) await reroll()
    }

    async function unReroll() {
        if ($doingChat || rerollBusy) return
        await runSelectedConversationOperation('previous-response-candidate', async (context) => {
            const { conversation, session } = context.requireCurrent()
            if (moveResponseCandidate(conversation, session, -1, v4)) {
                await persistentRuntime.flushPendingData('select-response-candidate')
            }
        })
    }

    async function writeSelectedConversationSuggestions(suggestions: readonly string[]): Promise<boolean> {
        const result = await runSelectedConversationOperation('write-auto-suggestions', (context) => {
            const authority = context.requireCurrent()
            writeConversationSuggestions(authority.conversation, authority.session, suggestions)
            context.requireCurrent()
            return true
        })
        return result === true
    }

    async function selectAlternateGreeting(direction: -1 | 1): Promise<void> {
        try {
            await runSelectedConversationOperation('select-alternate-greeting', (context) => {
                const { character, conversation, session } = context.requireCurrent()
                if (character.type === 'group') return
                moveAlternateGreeting(
                    conversation,
                    session,
                    character.alternateGreetings.length,
                    direction,
                )
                context.requireCurrent()
            })
        } catch (error) {
            alertError(error)
        }
    }

    async function removeCreatorQuote(): Promise<void> {
        const character = DBState.db.characters[$selectedCharID]
        if (!character || character.type === 'group') return
        try {
            const changed = await persistentRuntime.mutatePersistentCharacterDetail(
                character.chaId,
                'remove-creator-quote',
                ({ character: storedCharacter }) => {
                    if (storedCharacter.type !== 'group') storedCharacter.removedQuotes = true
                },
            )
            if (!changed) alertError(language.errors.noData)
        } catch (error) {
            alertError(error)
        }
    }

    let abortController:null|AbortController = null

    async function sendChatMain(continued:boolean = false) {
        return runSelectedConversationOperation(
            continued ? 'continue-generation' : 'generate-response',
            (context) => sendChatMainComplete(context, continued),
        )
    }

    async function sendChatMainComplete(
        context: ConversationOperationContext,
        continued: boolean = false,
    ) {
        requireConversationMutationTarget(context)
        messageInput = ''
        abortController = new AbortController()
        try {
            await sendChat(-1, {
                signal: abortController.signal,
                continue: continued,
            })
        } catch (error) {
            if (error instanceof SelectedConversationPromotionStaleError) return
            console.error(error)
            alertError(error)
        } finally {
            $doingChat = false
        }
        if (DBState.db.playMessage) {
            const audio = new Audio(sendSound)
            audio.play().catch(() => {})
        }
    }

    function abortChat(){
        if(abortController){
            abortController.abort()
        }
    }

    async function runAutoMode() {
        if(autoMode){
            autoMode = false
            return
        }
        const selectedChar = $selectedCharID
        autoMode = true
        while(autoMode){
            await sendChatMain()
            if(selectedChar !== $selectedCharID){
                autoMode = false
            }
        }
    }

    async function appendPlaygroundMessage() {
        return runSelectedConversationOperation(
            'append-playground-message',
            (context) => {
                const target = requireConversationMutationTarget(context)
                appendConversationMessage(target, {
                    role: 'char',
                    data: '',
                })
            },
        )
    }

    let { userIconPortrait, currentUsername, userIcon } = $derived.by(() => {
        const bindedPersona = DBState?.db?.characters?.[$selectedCharID]?.chats?.[DBState?.db?.characters?.[$selectedCharID]?.chatPage]?.bindedPersona

        if(bindedPersona){
            const persona = DBState.db.personas.find((p) => p.id === bindedPersona)
            if(persona){
                return {
                    currentUsername: persona.name,
                    userIconPortrait: persona.largePortrait,
                    userIcon: persona.icon
                }
            }
        }

        const selectedPersonaIndex = DBState.db.selectedPersona
        return {
            currentUsername: DBState.db.username,
            userIconPortrait: DBState.db.personas[selectedPersonaIndex].largePortrait,
            userIcon: DBState.db.personas[selectedPersonaIndex].icon
        }
    })

    let inputHeight = $state("44px")
    let inputEle:HTMLTextAreaElement = $state()
    let inputTranslateHeight = $state("44px")
    let inputTranslateEle:HTMLTextAreaElement = $state()

    function updateInputSizeAll() {
        updateInputSize()
        updateInputTranslateSize()
    }

    function updateInputTranslateSize() {
        if(inputTranslateEle) {
            inputTranslateEle.style.height = "0";
            inputTranslateHeight = (inputTranslateEle.scrollHeight) + "px";
            inputTranslateEle.style.height = inputTranslateHeight
        }
    }
    function updateInputSize() {
        if(inputEle){
            inputEle.style.height = "0";
            inputHeight = (inputEle.scrollHeight) + "px";
            inputEle.style.height = inputHeight
        }
    }

    $effect.pre(() => {
        updateInputSizeAll()
    });

    async function updateInputTransateMessage(reverse: boolean) {
        if(!DBState.db.useAutoTranslateInput){
            return
        }
        if(isExpTranslator()){
            if(!reverse){
                messageInputTranslate = ''
                return
            }
            if(messageInputTranslate === '') {
                messageInput = ''
                return
            }
            const lastMessageInputTranslate = messageInputTranslate
            await sleep(1500)
            if(lastMessageInputTranslate === messageInputTranslate){
                translate(reverse ? messageInputTranslate : messageInput, reverse).then((translatedMessage) => {
                    if(translatedMessage){
                        if(reverse)
                            messageInput = translatedMessage
                        else
                            messageInputTranslate = translatedMessage
                    }
                })
            }
            return

        }
        if(reverse && messageInputTranslate === '') {
            messageInput = ''
            return
        }
        if(!reverse && messageInput === '') {
            messageInputTranslate = ''
            return
        }
        translate(reverse ? messageInputTranslate : messageInput, reverse).then((translatedMessage) => {
            if(translatedMessage){
                if(reverse)
                    messageInput = translatedMessage
                else
                    messageInputTranslate = translatedMessage
            }
        })
    }

    /**
     * Opens a fresh source lease for the dialog. A lease is consumed by the job
     * it creates, so a retry after a failed capture has to acquire another one.
     */
    async function acquireScreenshotSource(): Promise<boolean> {
        const source = currentCharacter
        const chat = source?.chats[source.chatPage]
        if (!source || !chat) return false

        const openGeneration = ++screenshotOpenGeneration
        screenshotOpening = true
        try {
            const runtime = getPersistentDataRuntime()
            const lease = await openChatScreenshotSourceLease({
                characterId: source.chaId,
                chatId: chat.id ?? `${source.chaId}:${source.chatPage}`,
                renderContext: createScreenshotRenderContext(source, chat),
            }, {
                store: runtime.store,
                flushPendingData: (reason) => runtime.flushPendingData(reason),
                getNavigationGeneration: () => runtime.getNavigationGeneration(),
                getActiveConversationSession: () => runtime.getActiveConversationSession(),
                captureSelectedConversationTarget: () =>
                    runtime.captureSelectedConversationTarget(),
                captureSelectedConversationAuthority: () =>
                    runtime.captureSelectedConversationAuthority(),
            })
            if (openGeneration !== screenshotOpenGeneration) {
                await lease.close()
                return false
            }
            screenshotSourceLease = lease
            screenshotDialogSnapshot = lease.snapshot
            screenshotTotalTurns = lease.snapshot.totalTurns
            return true
        } catch (error) {
            if (openGeneration !== screenshotOpenGeneration) return false
            if (!(error instanceof DOMException && error.name === 'AbortError')) {
                const detail = error instanceof Error ? error.message : String(error)
                screenshotError = language.screenshotFailed.replace('{error}', detail)
                alertError(screenshotError)
            }
            return false
        } finally {
            if (openGeneration === screenshotOpenGeneration) screenshotOpening = false
        }
    }

    async function openScreenshotDialog() {
        if (
            screenshotOpening
            || screenshotRunning
            || screenshotDialogOpen
            || screenshotSourceLease
        ) return
        screenshotError = ''
        screenshotCompletedTurns = 0
        if (await acquireScreenshotSource()) screenshotDialogOpen = true
    }

    function cancelScreenshot() {
        screenshotController?.abort()
    }

    function releaseScreenshotSource() {
        const source = screenshotSourceLease
        screenshotSourceLease = null
        if (source) void source.close().catch(console.error)
    }

    function closeScreenshotDialog() {
        screenshotOpenGeneration += 1
        cancelScreenshot()
        screenshotDialogOpen = false
        screenshotDialogSnapshot = null
        releaseScreenshotSource()
    }

    function captureVariables(source: character | groupChat, chat: ChatRecord) {
        const variables = Object.fromEntries([
            ...parseKeyValue(DBState.db.templateDefaultVariables ?? ''),
            ...parseKeyValue(source.defaultVariables ?? ''),
        ])
        for (const [key, value] of Object.entries(chat.scriptstate ?? {})) {
            variables[key.replace(/^\$/, '')] = String(value)
        }
        return variables
    }

    function createCaptureParserContext(
        source: character | groupChat,
        chat: ChatRecord,
    ) {
        const character = snapshotChatScreenshotCharacter(source, chat)
        const memberIds = new Set(source.type === 'group' ? source.characters : [])
        const members = DBState.db.characters
            .filter((candidate) => candidate !== source && memberIds.has(candidate.chaId))
            .map((candidate) => snapshotChatScreenshotCharacter(
                candidate,
                candidate.chats[candidate.chatPage] ?? chat,
            ))
        const database = {
            characters: [character, ...members],
            mainPrompt: DBState.db.mainPrompt,
            jailbreak: DBState.db.jailbreak,
            globalNote: DBState.db.globalNote,
            jailbreakToggle: DBState.db.jailbreakToggle,
            maxContext: DBState.db.maxContext,
            aiModel: DBState.db.aiModel,
            subModel: DBState.db.subModel,
            language: DBState.db.language,
            promptTemplate: DBState.db.promptTemplate,
            translatorType: DBState.db.translatorType,
            translator: DBState.db.translator,
            translatorInputLanguage: DBState.db.translatorInputLanguage,
            translatorPrompt: DBState.db.translatorPrompt,
            translatorMaxResponse: DBState.db.translatorMaxResponse,
            translatorPresets: DBState.db.translatorPresets,
            translatorPresetId: DBState.db.translatorPresetId,
            htmlTranslation: DBState.db.htmlTranslation,
            combineTranslation: DBState.db.combineTranslation,
            playMessageOnTranslateEnd: DBState.db.playMessageOnTranslateEnd,
            useExperimentalGoogleTranslator: DBState.db.useExperimentalGoogleTranslator,
            noWaitForTranslate: DBState.db.noWaitForTranslate,
            deeplOptions: DBState.db.deeplOptions,
            deeplXOptions: DBState.db.deeplXOptions,
        } as Database
        const globalChatVariables = { ...(DBState.db.globalChatVariables ?? {}) }
        for (const [key, value] of Object.entries(chat.GLGlobalVariables ?? {})) {
            if (value && value !== 'null') globalChatVariables[key] = value
        }
        return {
            database,
            character,
            userName: currentUsername,
            personaPrompt: getPersonaPrompt(),
            modules: getModules(),
            moduleLorebooks: getModuleLorebooks(),
            selectedCharID: 0,
            chatVariables: captureVariables(source, chat),
            globalChatVariables,
            currentTime: Date.now(),
        }
    }

    function createScreenshotRenderContext(
        source: character | groupChat,
        chat: ChatRecord,
    ): ChatScreenshotRenderContext {
        return {
            character: createSimpleCharacter(source),
            characterName: source.name,
            characterImageSource: source.image,
            characterLargePortrait: source.type === 'group'
                ? false
                : source.largePortrait ?? false,
            userName: currentUsername,
            userImageSource: userIcon,
            userLargePortrait: userIconPortrait ?? false,
            moduleAssets: getModuleAssets(),
            presetRegex: DBState.db.presetRegex ?? [],
            moduleRegexScripts: getModuleRegexScripts(),
            assetStyle: source.prebuiltAssetStyle ?? '',
            parserContext: createCaptureParserContext(source, chat),
            settings: {
                autoTranslate: DBState.db.autoTranslate,
                autoTranslateCachedOnly: DBState.db.autoTranslateCachedOnly,
                translatorType: DBState.db.translatorType,
                translateBeforeHTMLFormatting: DBState.db.translateBeforeHTMLFormatting,
                legacyTranslation: DBState.db.legacyTranslation,
                showTranslationLoading: DBState.db.showTranslationLoading,
                newImageHandlingBeta: DBState.db.newImageHandlingBeta ?? false,
                assetWidth: DBState.db.assetWidth,
                hideAllImages: DBState.db.hideAllImages ?? false,
                iconSize: DBState.db.iconsize,
                zoomSize: DBState.db.zoomsize,
                lineHeight: DBState.db.lineHeight ?? 1.25,
                dynamicAssets: DBState.db.dynamicAssets,
                dynamicAssetsEditDisplay: DBState.db.dynamicAssetsEditDisplay,
                legacyMediaFindings: DBState.db.legacyMediaFindings ?? false,
                assetMaxDifference: DBState.db.assetMaxDifference,
                theme: DBState.db.theme,
                guiHTML: DBState.db.guiHTML,
                roundIcons: DBState.db.roundIcons,
                hideIcons: $HideIconStore,
                proseInvert: $ColorSchemeTypeStore === 'dark',
                requestInfoInsideChat: DBState.db.requestInfoInsideChat ?? false,
                aiLawApplies: aiLawApplies(),
                translator: DBState.db.translator,
                showFirstMessagePages: DBState.db.showFirstMessagePages,
                memoryLimitThickness: DBState.db.memoryLimitThickness ?? 1,
                customQuotes: DBState.db.customQuotes,
                customQuotesData: DBState.db.customQuotesData ?? ['“', '”', '‘', '’'],
                unformatQuotes: DBState.db.unformatQuotes,
                blockquoteStyling: DBState.db.blockquoteStyling ?? false,
                returnCSSError: DBState.db.returnCSSError ?? false,
            },
        }
    }

    function captureCurrentParserConversation(): CurrentChatMessageTarget | null {
        const character = DBState.db.characters[$selectedCharID]
        const conversation = character?.chats[character.chatPage]
        return character && conversation ? { character, conversation } : null
    }

    function createBoundedLiveParserContext(
        current: CurrentChatMessageTarget,
    ): ProcessScriptCaptureContext {
        return {
            presetRegex: DBState.db.presetRegex ?? [],
            moduleRegexScripts: getModuleRegexScripts(),
            moduleAssets: getModuleAssets(),
            dynamicAssets: DBState.db.dynamicAssets,
            dynamicAssetsEditDisplay: DBState.db.dynamicAssetsEditDisplay,
            parserContext: createCaptureParserContext(
                current.character,
                current.conversation,
            ),
        }
    }

    function createCompleteLiveParserContext(
        current: CurrentChatMessageTarget,
    ): ProcessScriptCaptureContext {
        return bindCompleteLiveParserContextAuthority(
            createBoundedLiveParserContext(current),
            current,
        )
    }

    function liveParserIndirections(
        current: CurrentChatMessageTarget,
    ): Readonly<Record<string, unknown>> {
        return createLiveChatParserIndirections(
            DBState.db,
            current.character,
            current.conversation,
            getPersonaPrompt(),
        )
    }

    const liveParserProjectionResolver = createSelectedConversationLiveParserProjectionResolver({
        runtime: persistentRuntime,
        maxProjectionMessages: 256,
        captureCurrent: captureCurrentParserConversation,
        createBoundedContextSeed: createBoundedLiveParserContext,
        createCompleteContext: createCompleteLiveParserContext,
        parserSource: (current) =>
            createLiveChatParserSource(DBState.db, current.character, getModuleRegexScripts()),
        parserIndirections: liveParserIndirections,
        unsafeDependencies: (current) => {
            const moduleTriggers = getModuleTriggers()
            const triggers = current.character.type === 'group'
                ? moduleTriggers
                : [...(current.character.triggerscript ?? []), ...moduleTriggers]
            const regexScripts = [
                ...(DBState.db.presetRegex ?? []),
                ...(current.character.customscript ?? []),
                ...getModuleRegexScripts(),
            ]
            return collectLiveChatParserUnsafeDependencies({
                triggers,
                pluginV2EditDisplay: pluginV2.editdisplay.size > 0,
                regexScripts,
            })
        },
    })

    async function startScreenshot(start: number, end: number) {
        const dialogSnapshot = screenshotDialogSnapshot
        const sourceLease = screenshotSourceLease
        if (
            screenshotRunning
            || !dialogSnapshot
            || !sourceLease
            || !screenshotSurface
        ) return

        const controller = new AbortController()
        screenshotController = controller
        screenshotRunning = true
        screenshotCompletedTurns = 0
        screenshotError = ''
        let retryableFailure = ''

        try {
            const job = await sourceLease.createJob(start, end, controller.signal)
            if (screenshotSourceLease === sourceLease) screenshotSourceLease = null
            screenshotTotalTurns = job.totalTurns

            const fileBase = `chat-${crypto.randomUUID()}`
            await captureChatScreenshot(job, {
                surface: screenshotSurface,
                encoder: createDomScreenshotEncoder(),
                signal: controller.signal,
                onProgress: ({ completedTurns }) => {
                    screenshotCompletedTurns = completedTurns
                },
                output: {
                    async publishPng(page) {
                        return downloadFile(`${fileBase}.png`, new Uint8Array(await page.arrayBuffer()))
                    },
                    async createArchive() {
                        const androidSafReady = isTauriAndroid && isAndroidSafFileJobsEnabled()
                        if (!canExportLongScreenshotArchive(
                            isTauri,
                            isTauriDesktop,
                            androidSafReady,
                        )) {
                            throw new Error(language.screenshotLongNativeUnavailable)
                        }
                        if (isTauriDesktop) {
                            const writer = await createNativeScreenshotArchiveWriter(`${fileBase}.zip`)
                            return createStreamingScreenshotArchive(writer)
                        }
                        if (androidSafReady) {
                            const writer = await createAndroidScreenshotArchiveWriter(`${fileBase}.zip`)
                            return createStreamingScreenshotArchive(writer)
                        }
                        const writer = new LocalWriter()
                        const selected = await writer.init('ZIP', ['zip'], `${fileBase}.zip`)
                        if (!selected) throw new DOMException('Screenshot export was cancelled', 'AbortError')
                        return createStreamingScreenshotArchive(writer)
                    },
                },
            })
            alertNormal(language.screenshotSaved)
            screenshotDialogOpen = false
            screenshotDialogSnapshot = null
        } catch (error) {
            if (screenshotSourceLease === sourceLease) screenshotSourceLease = null
            await sourceLease.close().catch(console.error)
            screenshotDialogSnapshot = null
            const partialDestinationMayRemain = !!(
                error
                && typeof error === 'object'
                && 'warningCodes' in error
                && Array.isArray((error as { warningCodes?: unknown }).warningCodes)
                && (error as { warningCodes: unknown[] }).warningCodes
                    .includes('partial-destination-may-remain')
            )
            if (
                !(error instanceof DOMException && error.name === 'AbortError')
                || partialDestinationMayRemain
            ) {
                console.error(error)
                const detail = describeScreenshotPublicationError(
                    error,
                    language.screenshotPartialDestinationMayRemain,
                )
                screenshotError = language.screenshotFailed.replace('{error}', detail)
                alertError(screenshotError)
                // Keep the dialog open so the failure is readable and the entered
                // range can be captured again without reopening the dialog.
                retryableFailure = screenshotError
            } else {
                screenshotDialogOpen = false
            }
        } finally {
            if (screenshotController === controller) screenshotController = null
            screenshotRunning = false
        }

        if (!retryableFailure || !screenshotDialogOpen) return
        if (await acquireScreenshotSource()) {
            screenshotError = retryableFailure
            return
        }
        screenshotDialogOpen = false
        screenshotDialogSnapshot = null
    }

    onDestroy(() => {
        selectedConversationViewport.dispose()
        screenshotOpenGeneration += 1
        cancelScreenshot()
        releaseScreenshotSource()
    })

    
</script>



<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="w-full h-full relative" style={customStyle} onclick={() => {
    openMenu = false
}}>
    <ChatScreenshotCaptureSurface bind:this={screenshotSurface} />

    {#if screenshotDialogOpen}
        <ChatScreenshotDialog
            totalTurns={screenshotTotalTurns}
            running={screenshotRunning}
            completedTurns={screenshotCompletedTurns}
            error={screenshotError}
            onStart={startScreenshot}
            onCancel={cancelScreenshot}
            onClose={closeScreenshotDialog}
        />
    {/if}
    
    {#if showNewMessageButton}
        {#if (DBState.db.newMessageButtonStyle === 'bottom-center' || !DBState.db.newMessageButtonStyle)}
            <button class="absolute bottom-16 left-1/2 -translate-x-1/2 bg-blue-500 text-white px-4 py-2 rounded-full shadow-lg z-50 flex items-center gap-2 hover:bg-blue-600 transition-colors" onclick={scrollToBottom}>
                <ArrowDown size={16} />
                <span>{language.newMessage}</span>
            </button>
        {/if}

        {#if DBState.db.newMessageButtonStyle === 'bottom-right'}
            <button class="absolute bottom-20 right-4 bg-blue-500 text-white px-4 py-2 rounded-full shadow-lg z-50 flex items-center gap-2 hover:bg-blue-600 transition-colors" onclick={scrollToBottom}>
                <ArrowDown size={16} />
                <span>{language.newMessage}</span>
            </button>
        {/if}

        {#if DBState.db.newMessageButtonStyle === 'bottom-left'}
            <button class="absolute bottom-20 left-4 bg-blue-500 text-white px-4 py-2 rounded-full shadow-lg z-50 flex items-center gap-2 hover:bg-blue-600 transition-colors" onclick={scrollToBottom}>
                <ArrowDown size={16} />
                <span>{language.newMessage}</span>
            </button>
        {/if}

        {#if DBState.db.newMessageButtonStyle === 'floating-circle'}
            <button class="absolute bottom-36 right-4 bg-blue-500 text-white w-12 h-12 rounded-full shadow-lg z-50 flex items-center justify-center hover:bg-blue-600 transition-colors" onclick={scrollToBottom} title="4. 원형 (우하단)">
                <ArrowDown size={20} />
            </button>
        {/if}

        {#if DBState.db.newMessageButtonStyle === 'right-center'}
            <button class="absolute top-1/2 right-2 -translate-y-1/2 bg-blue-500 text-white px-2 py-3 rounded-l-lg shadow-lg z-50 flex flex-col items-center gap-1 hover:bg-blue-600 transition-colors" onclick={scrollToBottom}>
                <ArrowDown size={14} />
                <span class="text-xs writing-mode-vertical">{language.newMessage}</span>
            </button>
        {/if}

        {#if DBState.db.newMessageButtonStyle === 'top-bar'}
            <button class="absolute top-2 left-1/2 -translate-x-1/2 bg-blue-500 text-white px-6 py-1.5 rounded-full shadow-lg z-50 flex items-center gap-2 hover:bg-blue-600 transition-colors text-sm" onclick={scrollToBottom}>
                <ArrowDown size={14} />
                <span>{language.newMessage}</span>
            </button>
        {/if}
    {/if}
    {#if isScrollingToMessage}
        <div class="absolute inset-0 z-50 flex items-center justify-center bg-black/50 text-white text-xl font-bold backdrop-blur-sm">
            Loading...
        </div>
    {/if}
    {#if $selectedCharID < 0}
        {#if $PlaygroundStore === 0}
            <MainMenu />
        {:else}
            {#await loadPlaygroundMenu() then PlaygroundMenu}
                <PlaygroundMenu />
            {/await}
        {/if}
    {:else}
        <div class="h-full w-full flex flex-col-reverse overflow-y-auto relative default-chat-screen" onscroll={(e) => {
            const chatTarget = e.target as HTMLElement;
            const latestMessage = chatTarget.querySelector<HTMLElement>('.is-latest-chat-row');
            const isAtBottom = latestMessage
                ? latestMessage.getBoundingClientRect().top <= chatTarget.getBoundingClientRect().bottom + 100
                : true;
            if(isAtBottom){
                showNewMessageButton = false;
            }
        }}>
            <div
                    class="{DBState.db.fixedChatTextarea ? 'sticky pt-2 pb-2 right-0 bottom-0 bg-bgcolor' : 'mt-2 mb-2'} flex items-stretch w-full"
                    style="{DBState.db.fixedChatTextarea ? 'z-index:29;' : ''}"
            >
                {#if DBState.db.useChatSticker && currentCharacter.type !== 'group'}
                    <div onclick={()=>{toggleStickers = !toggleStickers}}
                         class={"ml-4 bg-textcolor2 flex justify-center items-center  w-12 h-12 rounded-md hover:bg-blue-500 transition-colors "+(toggleStickers ? 'text-green-500':'text-textcolor')}>
                        <Laugh/>
                    </div>
                {/if}

                <textarea class="peer text-input-area focus:border-textcolor transition-colors outline-hidden text-textcolor p-2 min-w-0 border border-r-0 bg-transparent rounded-md rounded-r-none input-text text-xl grow ml-4 border-darkborderc resize-none overflow-y-hidden overflow-x-hidden max-w-full placeholder:text-sm"
                          bind:value={messageInput}
                          bind:this={inputEle}
                          onkeydown={(e) => {
                        if(e.key.toLocaleLowerCase() === "enter" && !e.isComposing){
                            if(DBState.db.sendWithEnter && (!e.shiftKey)){
                                send()
                                e.preventDefault()
                            }else if(!DBState.db.sendWithEnter && e.shiftKey){
                                send()
                                e.preventDefault()
                            }
                        }
                        if(e.key.toLocaleLowerCase() === "m" && (e.ctrlKey)){
                            reroll()
                            e.preventDefault()
                        }
                    }}
                          onpaste={(e) => {
                        const items = e.clipboardData?.items
                        if(!items){
                            return
                        }
                        let canceled = false

                        for(const item of items){
                            if(item.kind === 'file' && item.type.startsWith('image')){
                                if(!canceled){
                                    e.preventDefault()
                                    canceled = true
                                }
                                const file = item.getAsFile()
                                if(file){
                                    const reader = new FileReader()
                                    reader.onload = async (e) => {
                                        const buf = e.target?.result as ArrayBuffer
                                        const uint8 = new Uint8Array(buf)
                                        const results = await postChatFile({
                                            name: file.name,
                                            data: uint8
                                        })
                                        if(!results) return
                                        for(const res of results){
                                            if(res?.type === 'asset'){
                                                fileInput.push(res.data)
                                            }
                                            if(res?.type === 'text'){
                                                messageInput += `{{file::${res.name}::${res.data}}}`
                                            }
                                        }
                                        updateInputSizeAll()
                                    }
                                    reader.readAsArrayBuffer(file)
                                }
                            }
                        }
                    }}
                          oninput={()=>{updateInputSizeAll();updateInputTransateMessage(false)}}
                          style:height={inputHeight}
                ></textarea>


                {#if $doingChat || doingChatInputTranslate}
                    <button
                            aria-labelledby="cancel"
                            class="peer-focus:border-textcolor  flex justify-center border-y border-darkborderc items-center text-textcolor p-3 hover:bg-blue-500 hover:text-white transition-colors" onclick={abortChat}
                            style:height={inputHeight}
                    >
                        <div class="loadmove chat-process-stage-{$chatProcessStage}" class:autoload={autoMode}></div>
                    </button>
                {:else}
                    <button
                            onclick={send}
                            class="flex justify-center border-y border-darkborderc items-center text-textcolor p-3 peer-focus:border-textcolor hover:bg-blue-500 hover:text-white transition-colors button-icon-send"
                            style:height={inputHeight}
                    >
                        <Send />
                    </button>
                {/if}
                {#if DBState.db.characters[$selectedCharID]?.chaId !== '§playground'}
                    <button
                            onclick={(e) => {
                            openMenu = !openMenu
                            e.stopPropagation()
                        }}
                            class="peer-focus:border-textcolor mr-2 flex border-y border-r border-darkborderc justify-center items-center text-textcolor p-3 rounded-r-md hover:bg-blue-500 hover:text-white transition-colors"
                            style:height={inputHeight}
                    >
                        <MenuIcon />
                    </button>
                {:else}
                    <div onclick={() => appendPlaygroundMessage()}
                         class="peer-focus:border-textcolor mr-2 flex border-y border-r border-darkborderc justify-center items-center text-textcolor p-3 rounded-r-md hover:bg-blue-500 hover:text-white transition-colors"
                         style:height={inputHeight}
                    >
                        <Plus />
                    </div>
                {/if}
            </div>
            {#if DBState.db.useAutoTranslateInput && DBState.db.characters[$selectedCharID]?.chaId !== '§playground'}
                <div class="flex items-center mt-2 mb-2">
                    <label for='messageInputTranslate' class="text-textcolor ml-4">
                        <LanguagesIcon />
                    </label>
                    <textarea id = 'messageInputTranslate' class="text-textcolor rounded-md p-2 min-w-0 bg-transparent input-text text-xl grow ml-4 mr-2 border-darkbutton resize-none focus:bg-selected overflow-y-hidden overflow-x-hidden max-w-full"
                              bind:value={messageInputTranslate}
                              bind:this={inputTranslateEle}
                              onkeydown={(e) => {
                            if(e.key.toLocaleLowerCase() === "enter" && (!e.shiftKey) && !e.isComposing){
                                if(DBState.db.sendWithEnter){
                                    send()
                                    e.preventDefault()
                                }
                            }
                            if(e.key.toLocaleLowerCase() === "m" && (e.ctrlKey)){
                                reroll()
                                e.preventDefault()
                            }
                        }}
                              oninput={()=>{updateInputSizeAll();updateInputTransateMessage(true)}}
                              placeholder={language.enterMessageForTranslateToEnglish}
                              style:height={inputTranslateHeight}
                    ></textarea>
                </div>
            {/if}

            {#if fileInput.length > 0}
                <div class="flex items-center ml-4 flex-wrap p-2 m-2 border-darkborderc border rounded-md">
                    {#each fileInput as file, i (file)}
                        <div class="relative">
                            <InlayFilePreview id={file} />
                            <button class="absolute -right-1 -top-1 p-1 bg-darkbg text-textcolor rounded-md transition-colors hover:text-draculared focus:text-draculared" onclick={() => {
                                fileInput.splice(i, 1)
                                updateInputSizeAll()
                            }}>
                                <XIcon size={18} />
                            </button>
                        </div>
                    {/each}
                </div>

            {/if}

            {#if toggleStickers}
                <div class="ml-4 flex flex-wrap">
                    <AssetInput currentCharacter={currentCharacter} onSelect={(additionalAsset)=>{
                        let fileType = 'img'
                        if(additionalAsset.length > 2 && additionalAsset[2]) {
                            const fileExtension = additionalAsset[2]
                            if(fileExtension === 'mp4' || fileExtension === 'webm')
                                fileType = 'video'
                            else if(fileExtension === 'mp3' || fileExtension === 'wav')
                                fileType = 'audio'
                        }
                        messageInput += `<span class='notranslate' translate='no'>{{${fileType}::${additionalAsset[0]}}}</span> *${additionalAsset[0]} added*`
                        updateInputSizeAll()
                    }}/>
                </div>
            {/if}

            {#if DBState.db.useAutoSuggestions}
                <Suggestion
                    messageInput={(msg) =>
                        (messageInput =
                            (DBState.db.subModel === 'textgen_webui' ||
                                DBState.db.subModel === 'mancer') &&
                            DBState.db.autoSuggestClean
                                ? msg.replace(/ +\(.+?\) *$| - [^"'*]*?$/, '')
                                : msg)}
                    {send}
                    readLatestMessages={(signal) =>
                        readSelectedConversationLatestTail(
                            persistentRuntime,
                            10,
                            signal,
                        )}
                    writeSuggestions={writeSelectedConversationSuggestions}
                    getNavigationGeneration={() =>
                        persistentRuntime.getNavigationGeneration()}
                />
            {/if}

            {#if chatPanelStore.length > 0}
                <div class="mx-4 my-2 flex flex-col gap-2">
                    {#each chatPanelStore as panel (panel.id)}
                        <section class={`rounded-md border border-darkborderc bg-darkbg/80 p-3 text-textcolor ${panel.className ?? ''}`} data-plugin-chat-panel={panel.id}>
                            {@html panel.html}
                        </section>
                    {/each}
                </div>
            {/if}

            {#if chatFoldedStateMessageIndex.index !== -1}
                <button class="w-full flex justify-center max-w-full p-4">
                    <Button className="max-w-xl w-full" onclick={() => {
                        chatFoldedState.data = null
                        void chatsInstance?.jumpToLatestMessage()
                    }}>
                        {language.loadMore}
                    </Button>
                </button>
            {/if}
            
            <Chats
                bind:this={chatsInstance}
                messages={conversationViewportSource ? undefined : currentChat}
                viewportSource={conversationViewportSource}
                viewportNavigationGeneration={conversationViewportNavigationGeneration}
                parserProjectionResolver={liveParserProjectionResolver}
                acquireConversationStartParserLease={liveParserProjectionResolver.acquireConversationStart}
                selectedConversationOperations={conversationViewportSource
                    ? selectedConversationOperations
                    : undefined}
                onReroll={reroll}
                onNextReroll={nextReroll}
                unReroll={unReroll}
                onFirstMessageReroll={() => void selectAlternateGreeting(1)}
                unFirstMessageReroll={() => void selectAlternateGreeting(-1)}
                onRemoveCreatorQuote={() => void removeCreatorQuote()}
                showAiWarning={aiLawApplies()}
                currentCharacter={currentCharacter}
                currentUsername={currentUsername}
                userIcon={userIcon}
                userIconPortrait={userIconPortrait}
                bind:hasNewUnreadMessage={showNewMessageButton}
            />

            {#if openMenu}
                <div class="{DBState.db.fixedChatTextarea ? 'fixed' : 'absolute'} right-2 bottom-16 p-5 bg-darkbg flex flex-col gap-3 text-textcolor rounded-md" onclick={(e) => {
                    e.stopPropagation()
                }}>
                    {#if DBState.db.characters[$selectedCharID].type === 'group'}
                        <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={runAutoMode}>
                            <DicesIcon />
                            <span class="ml-2">{language.autoMode}</span>
                        </div>
                    {/if}

                    
                    <!-- svelte-ignore block_empty -->
                    {#if DBState.db.characters[$selectedCharID].ttsMode === 'webspeech' || DBState.db.characters[$selectedCharID].ttsMode === 'elevenlab'}
                        <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                            stopTTS()
                        }}>
                            <MicOffIcon />
                            <span class="ml-2">{language.ttsStop}</span>
                        </div>
                    {/if}

                    <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors"
                        class:text-textcolor2={!canContinueResponse}
                        onclick={() => {
                            if (!canContinueResponse) return
                            sendContinue();
                        }}
                    >
                        <StepForwardIcon />
                        <span class="ml-2">{language.continueResponse}</span>
                    </div>


                    {#if DBState.db.showMenuChatList}
                        <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                            openChatList = true
                            openMenu = false
                        }}>
                            <DatabaseIcon />
                            <span class="ml-2">{language.chatList}</span>
                        </div>
                    {/if}

                    
                    {#if DBState.db.enableRisuaiProTools}
                        <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                            easyPanelStore.open = !easyPanelStore.open
                        }}>
                            <SparkleIcon />
                            <span class="ml-2">{language.easyPanel}</span>
                        </div>
                    {/if}

                    {#each additionalChatMenu as menu}
                        <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                            menu.callback()
                            openMenu = false
                        }}>
                            <PluginDefinedIcon ico={menu} />
                            <span class="ml-2">{menu.name}</span>
                        </div>
                    {/each}

                    {#if DBState.db.showMenuHypaMemoryModal}
                        {#if (DBState.db.supaModelType !== 'none' && DBState.db.hypav2) || DBState.db.hypaV3}
                            <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                                if (DBState.db.hypav2) {
                                    DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].hypaV2Data ??= {
                                        lastMainChunkID: 0,
                                        mainChunks: [],
                                        chunks: [],
                                    }
                                    showHypaV2Alert();
                                } else if (DBState.db.hypaV3) {
                                    $hypaV3ModalOpen = true
                                }

                                openMenu = false
                            }}>
                                <BrainIcon />
                                <span class="ml-2">
                                    {DBState.db.hypav2 ? language.hypaMemoryV2Modal : language.hypaMemoryV3Modal}
                                </span>
                            </div>
                        {/if}
                    {/if}
                    
                    {#if DBState.db.translator !== ''}
                        <div class={"flex items-center cursor-pointer "+ (DBState.db.useAutoTranslateInput ? 'text-green-500':'lg:hover:text-green-500')} onclick={() => {
                            DBState.db.useAutoTranslateInput = !DBState.db.useAutoTranslateInput
                        }}>
                            <GlobeIcon />
                            <span class="ml-2">{language.autoTranslateInput}</span>
                        </div>
                        
                    {/if}
            
                    <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                        openScreenshotDialog()
                    }}>
                        <CameraIcon />
                        <span class="ml-2">{language.screenshot}</span>
                    </div>

                    <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={async () => {
                        const results = await postChatFile(messageInput)
                        if(!results) return
                        for(const res of results){
                            if(res?.type === 'asset'){
                                fileInput.push(res.data)
                            }
                            if(res?.type === 'text'){
                                messageInput += `{{file::${res.name}::${res.data}}}`
                            }
                        }
                        updateInputSizeAll()
                    }}>

                        <ImagePlusIcon />
                        <span class="ml-2">{language.postFile}</span>
                    </div>


                    <div class={"flex items-center cursor-pointer "+ (DBState.db.useAutoSuggestions ? 'text-green-500':'lg:hover:text-green-500')} onclick={async () => {
                        DBState.db.useAutoSuggestions = !DBState.db.useAutoSuggestions
                    }}>
                        <ReplyIcon />
                        <span class="ml-2">{language.autoSuggest}</span>
                    </div>


                    <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={() => {
                        DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].modules ??= []
                        openModuleList = true
                        openMenu = false
                    }}>
                        <PackageIcon />
                        <span class="ml-2">{language.modules}</span>
                    </div>

                    {#if DBState.db.sideMenuRerollButton}
                        <div class="flex items-center cursor-pointer hover:text-green-500 transition-colors" onclick={reroll}>
                            <RefreshCcwIcon />
                            <span class="ml-2">{language.reroll}</span>
                        </div>
                    {/if}
                </div>

            {/if}
        </div>

    {/if}
</div>

{#if additionalFloatingActionButtons.length > 0}
    <div class="fixed top-4 right-4 flex flex-col gap-3 z-50">
        {#each additionalFloatingActionButtons as button}
            <button class="bg-blue-500 text-white px-4 py-2 rounded-full shadow-lg flex items-center gap-2 hover:bg-blue-600 transition-colors" onclick={() => {
                button.callback()
            }}>
                <PluginDefinedIcon ico={button} />
            </button>
        {/each}
    </div>
{/if}
<style>

    .chat-process-stage-1{
        border-top: 0.4rem solid #60a5fa;
        border-left: 0.4rem solid #60a5fa;
    }

    .chat-process-stage-2{
        border-top: 0.4rem solid #db2777;
        border-left: 0.4rem solid #db2777;
    }

    .chat-process-stage-3{
        border-top: 0.4rem solid #34d399;
        border-left: 0.4rem solid #34d399;
    }

    .chat-process-stage-4{
        border-top: 0.4rem solid #8b5cf6;
        border-left: 0.4rem solid #8b5cf6;
    }

    .autoload{
        border-top: 0.4rem solid #10b981;
        border-left: 0.4rem solid #10b981;
    }

    @keyframes spin {
        
        0% { transform: rotate(0deg); }
        100% { transform: rotate(360deg); }
    }
</style>
