<script lang="ts">

    import Suggestion from './Suggestion.svelte';
    import { CameraIcon, DatabaseIcon, DicesIcon, GlobeIcon, ImagePlusIcon, LanguagesIcon, Laugh, MenuIcon, MicOffIcon, PackageIcon, Plus, RefreshCcwIcon, ReplyIcon, Send, StepForwardIcon, XIcon, BrainIcon, ArrowDown, SparkleIcon } from "@lucide/svelte";
    import { selectedCharID, PlaygroundStore, createSimpleCharacter, hypaV3ModalOpen, ScrollToMessageStore, additionalChatMenu, additionalFloatingActionButtons, easyPanelStore, chatPanelStore } from "../../ts/stores.svelte";
    import { onDestroy, tick } from 'svelte';
    import Chat from "./Chat.svelte";
    import { type Chat as ChatRecord, type Database, type character, type groupChat, type Message } from "../../ts/storage/database.svelte";
    import { DBState } from 'src/ts/stores.svelte';
    import { getCharImage } from "../../ts/characters";
    import { chatProcessStage, doingChat, sendChat } from "../../ts/process/index.svelte";
    import { getPersonaPrompt, parseKeyValue, sleep } from "../../ts/util";
    import { language } from "../../lang";
    import { isExpTranslator, translate } from "../../ts/translator/translator";
    import { alertError, alertNormal, showHypaV2Alert } from "../../ts/alert";
    import sendSound from '../../etc/send.mp3'
    import { processScript } from "src/ts/process/scripts";
    import CreatorQuote from "./CreatorQuote.svelte";
    import { stopTTS } from "src/ts/process/tts";
    import MainMenu from '../UI/MainMenu.svelte';
    import AssetInput from './AssetInput.svelte';
    import { aiLawApplies, chatFoldedState, chatFoldedStateMessageIndex, downloadFile, LocalWriter } from 'src/ts/globalApi.svelte';
    import { runTrigger } from 'src/ts/process/triggers';
    import { PreUnreroll, Prereroll } from 'src/ts/process/prereroll';
    import { processMultiCommand } from 'src/ts/process/command';
    import { postChatFile } from 'src/ts/process/files/multisend';
    import InlayFilePreview from './InlayFilePreview.svelte';
    import { ConnectionOpenStore } from 'src/ts/sync/multiuser';
    import { coldStorageHeader, preLoadChat } from 'src/ts/process/coldstorage.svelte';
    import Chats from './Chats.svelte';
    import Button from '../UI/GUI/Button.svelte';
    import PluginDefinedIcon from '../Others/PluginDefinedIcon.svelte';
    import { getAdditionalChatLoadPages, getInitialChatLoadPages } from 'src/ts/chatLoadPages';
    import { getActiveConversationSession } from '../../ts/storage/persistentDataRuntime.svelte';
    import {
        appendConversationMessage,
        captureConversationMutationTarget,
        isConversationMutationOwnerCurrent,
        isConversationMutationTargetCurrent,
        refreshConversationMutationTarget,
        type ConversationMutationTarget,
    } from '../../ts/conversationMutations';
    import { appendDefaultChatInput } from './defaultChatInput';
    import {
        appendConversationRerollHistory,
        captureConversationRerollTail,
        createConversationRerollHistory,
        isConversationRerollHistoryCurrent,
        moveConversationRerollHistory,
        refreshConversationRerollHistory,
        replaceConversationRerollLastData,
        truncateConversationForReroll,
        type ConversationRerollHistory,
    } from '../../ts/conversationReroll';
    import {
        LatestChatScrollRequestGuard,
        resolveChatMessageTarget,
        type CapturedChatMessageTarget,
    } from '../../ts/chatMessageUi';
    import { handleDefaultChatUnreroll } from './defaultChatReroll';
    import ChatScreenshotDialog from './ChatScreenshotDialog.svelte';
    import ChatScreenshotCaptureSurface from './ChatScreenshotCaptureSurface.svelte';
    import {
        createChatScreenshotDialogSnapshot,
        createChatScreenshotJobFromDialogSnapshot,
        snapshotChatScreenshotCharacter,
        type ChatScreenshotDialogSnapshot,
        type ChatScreenshotRenderContext,
    } from 'src/ts/chatScreenshotRange';
    import { canExportLongScreenshotArchive, captureChatScreenshot, createDomScreenshotEncoder, type ChatScreenshotSurface } from 'src/ts/chatScreenshotCapture';
    import { createStreamingScreenshotArchive } from 'src/ts/chatScreenshotArchive';
    import { isTauri } from 'src/ts/platform';
    import { getModuleAssets, getModuleLorebooks, getModuleRegexScripts, getModules } from 'src/ts/process/modules';
    import { ColorSchemeTypeStore } from 'src/ts/gui/colorscheme';
    import { HideIconStore } from 'src/ts/stores.svelte';

    const loadPlaygroundMenu = () => import('../Playground/PlaygroundMenu.svelte').then(m => m.default);
    
    interface Props {
        openModuleList?: boolean;
        openChatList?: boolean;
        customStyle?: string;
    }

    let messageInput:string = $state('')
    let messageInputTranslate:string = $state('')
    let openMenu = $state(false)
    let loadPages = $state(getInitialChatLoadPages(DBState.db))
    let autoMode = $state(false)
    let rerollHistory:ConversationRerollHistory|null = null
    let doingChatInputTranslate = false
    let toggleStickers:boolean = $state(false)
    let fileInput:string[] = $state([])
    let showNewMessageButton = $state(false)
    let chatsInstance: any = $state()
    let isScrollingToMessage = $state(false)
    let { openModuleList = $bindable(false), openChatList = $bindable(false), customStyle = '' }: Props = $props();
    let currentCharacter = $derived(DBState.db.characters[$selectedCharID])
    let currentChat = $derived(currentCharacter?.chats[currentCharacter.chatPage]?.message ?? [])
    const scrollRequestGuard = new LatestChatScrollRequestGuard()

    function captureCurrentConversationTarget(): ConversationMutationTarget | null {
        const character = DBState.db.characters[$selectedCharID]
        const conversation = character?.chats[character.chatPage]
        if (!character || !conversation) return null
        return captureConversationMutationTarget(
            character,
            conversation,
            getActiveConversationSession(),
        )
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

    function conversationOwnerIsCurrent(target: ConversationMutationTarget): boolean {
        const character = DBState.db.characters[$selectedCharID]
        return isConversationMutationOwnerCurrent(
            target,
            character,
            character?.chats[character.chatPage],
            getActiveConversationSession(),
        )
    }

    function refreshCurrentConversationTarget(
        target: ConversationMutationTarget,
    ): ConversationMutationTarget | null {
        const character = DBState.db.characters[$selectedCharID]
        return refreshConversationMutationTarget(
            target,
            character,
            character?.chats[character.chatPage],
            getActiveConversationSession(),
        )
    }

    function getCurrentRerollHistory(
        target: ConversationMutationTarget,
    ): ConversationRerollHistory | null {
        if (!rerollHistory) return null
        if (!isConversationRerollHistoryCurrent(rerollHistory, target)) {
            rerollHistory = null
            return null
        }
        return rerollHistory
    }

    function refreshRerollHistoryAfterOwnedMutation(
        target: ConversationMutationTarget,
    ): void {
        if (!rerollHistory) return
        rerollHistory = refreshConversationRerollHistory(rerollHistory, target)
    }
    let screenshotDialogOpen = $state(false)
    let screenshotTotalTurns = $state(0)
    let screenshotRunning = $state(false)
    let screenshotCompletedTurns = $state(0)
    let screenshotError = $state('')
    let screenshotController: AbortController | null = null
    let screenshotSurface: ChatScreenshotSurface | undefined
    let screenshotDialogSnapshot: ChatScreenshotDialogSnapshot | null = null

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
        if($ScrollToMessageStore){
            const target = $ScrollToMessageStore
            ScrollToMessageStore.set(null)
            scrollToMessage(target, scrollRequestGuard.begin())
        }
    })

    async function scrollToMessage(
        target: CapturedChatMessageTarget,
        requestGeneration: number,
    ){
        // Forces the loading of past messages not rendered on the screen
        isScrollingToMessage = true
        try {
            if (!scrollRequestGuard.isCurrent(requestGeneration)) return
            const resolved = resolveChatMessageTarget(target, scrollTargetContext)
            if (!resolved) return
            const index = resolved.absoluteIndex
            const totalMessages = currentChat.length
            const neededLoadPages = totalMessages - index + 5

            if(loadPages < neededLoadPages){
                loadPages = neededLoadPages
                await tick()
                if (
                    !scrollRequestGuard.isCurrent(requestGeneration) ||
                    !resolveChatMessageTarget(target, scrollTargetContext)
                ) return
            }

            let element: Element | null = null;
            // Poll for element existence (max 5 seconds)
            for(let i = 0; i < 50; i++){
                element = document.querySelector(`[data-chat-index="${index}"]`)
                if(element) break;
                await sleep(100)
                if (
                    !scrollRequestGuard.isCurrent(requestGeneration) ||
                    !resolveChatMessageTarget(target, scrollTargetContext)
                ) return
            }

            const preIndex = Math.max(0, index - 3)
            const preElement = document.querySelector(`[data-chat-index="${preIndex}"]`)
            if(preElement){
                preElement.scrollIntoView({behavior: "instant", block: "start"})
            } else {
                element?.scrollIntoView({behavior: "instant", block: "start"})
            }
            await sleep(50)
            if (
                !scrollRequestGuard.isCurrent(requestGeneration) ||
                !resolveChatMessageTarget(target, scrollTargetContext)
            ) return

            if(element){
                // Wait for images to load to prevent layout shift
                const chatContainer = document.querySelector('.default-chat-screen');
                if(chatContainer) {
                    const images = Array.from(chatContainer.querySelectorAll('img'));
                    const promises = images.map(img => {
                        if (img.complete) return Promise.resolve();
                        return new Promise(resolve => {
                            img.onload = () => resolve(null);
                            img.onerror = () => resolve(null);
                        });
                    });
                    // Wait for all images or timeout after 4 seconds
                    await Promise.race([
                        Promise.all(promises),
                        sleep(4000)
                    ]);
                    if (
                        !scrollRequestGuard.isCurrent(requestGeneration) ||
                        !resolveChatMessageTarget(target, scrollTargetContext)
                    ) return
                }

                element.scrollIntoView({behavior: "instant", block: "start"})
                
                // Small delay and scroll again to ensure position is correct after any final layout adjustments
                await sleep(50)
                if (
                    !scrollRequestGuard.isCurrent(requestGeneration) ||
                    !resolveChatMessageTarget(target, scrollTargetContext)
                ) return
                element.scrollIntoView({behavior: "instant", block: "start"})

                element.classList.add('ring-2', 'ring-blue-500')
                setTimeout(() => {
                    element.classList.remove('ring-2', 'ring-blue-500')
                }, 2000)
            }
        } finally {
            if (scrollRequestGuard.isCurrent(requestGeneration)) {
                isScrollingToMessage = false
            }
        }
    }

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
        let mutationTarget = captureCurrentConversationTarget()
        if (!mutationTarget) return
        const character = mutationTarget.character
        let messages = mutationTarget.conversation.message

        if(messageInput.startsWith('/')){
            const commandProcessed = await processMultiCommand(messageInput)
            if(commandProcessed !== false){
                messageInput = ''
                return
            }
            if (!conversationTargetIsCurrent(mutationTarget)) return
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
                    runInputTrigger: () => runTrigger(
                        character,
                        'input',
                        { chat: mutationTarget.conversation },
                    ),
                    processInput: () => processScript(character, messageInput, 'editinput'),
                    isTargetCurrent: () => conversationTargetIsCurrent(mutationTarget),
                    createMessage: (data) => ({
                        role: 'user',
                        data,
                        time: Date.now(),
                        name: $ConnectionOpenStore ? DBState.db.username : null
                    }),
                })
                if (!appended) return
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
        rerollHistory = null
        const refreshedTarget = refreshCurrentConversationTarget(mutationTarget)
        if (!refreshedTarget) return
        mutationTarget = refreshedTarget
        await sleep(10)
        if (!conversationTargetIsCurrent(mutationTarget)) return
        updateInputSizeAll()
        await sendChatMain(continueResponse)

    }

    async function reroll() {
        if($doingChat){
            return
        }
        const mutationTarget = captureCurrentConversationTarget()
        if (!mutationTarget) return
        let history = getCurrentRerollHistory(mutationTarget)
        const genId = mutationTarget.conversation.message.at(-1)?.generationInfo?.generationId
        if(genId){
            const r = Prereroll(genId)
            if(r){
                replaceConversationRerollLastData(mutationTarget, r, 'reroll')
                const refreshedTarget = refreshCurrentConversationTarget(mutationTarget)
                if (refreshedTarget) refreshRerollHistoryAfterOwnedMutation(refreshedTarget)
                else rerollHistory = null
                return
            }
        }
        if(history?.forward){
            rerollHistory = moveConversationRerollHistory(
                history,
                mutationTarget,
                'reroll',
            )
            return
        }
        if(!history){
            const messages = mutationTarget.conversation.message
            const tail = messages.length > 0
                ? captureConversationRerollTail(mutationTarget, messages.length - 1)
                : [undefined as Message]
            history = createConversationRerollHistory(mutationTarget, tail)
            rerollHistory = history
        }
        if (!truncateConversationForReroll(mutationTarget)) return
        const truncatedTarget = refreshCurrentConversationTarget(mutationTarget)
        if (!truncatedTarget) {
            rerollHistory = null
            return
        }
        rerollHistory = refreshConversationRerollHistory(history, truncatedTarget)
        if (!rerollHistory) return
        openMenu = false
        await sendChatMain()
    }

    async function unReroll() {
        if($doingChat){
            return
        }
        const mutationTarget = captureCurrentConversationTarget()
        if (!mutationTarget) return
        const history = getCurrentRerollHistory(mutationTarget)
        const result = handleDefaultChatUnreroll({
            target: mutationTarget,
            history,
            preUnreroll: PreUnreroll,
        })
        if (result.type === 'precomputed') {
            const refreshedTarget = refreshCurrentConversationTarget(mutationTarget)
            if (refreshedTarget) refreshRerollHistoryAfterOwnedMutation(refreshedTarget)
            else rerollHistory = null
            return
        }
        if (result.type === 'history') rerollHistory = result.history
    }

    let abortController:null|AbortController = null

    async function sendChatMain(continued:boolean = false) {
        const mutationTarget = captureCurrentConversationTarget()
        if (!mutationTarget) return
        const previousLength = mutationTarget.conversation.message.length
        messageInput = ''
        abortController = new AbortController()
        try {
            await sendChat(-1, {
                signal:abortController.signal,
                continue:continued
            })
            const refreshedTarget = conversationOwnerIsCurrent(mutationTarget)
                ? refreshCurrentConversationTarget(mutationTarget)
                : null
            if (
                refreshedTarget &&
                previousLength < refreshedTarget.conversation.message.length
            ) {
                const tail = captureConversationRerollTail(refreshedTarget, previousLength)
                const refreshedHistory = rerollHistory
                    ? refreshConversationRerollHistory(rerollHistory, refreshedTarget)
                    : null
                rerollHistory = refreshedHistory
                    ? appendConversationRerollHistory(refreshedHistory, refreshedTarget, tail)
                    : createConversationRerollHistory(refreshedTarget, tail)
            } else if (refreshedTarget) {
                refreshRerollHistoryAfterOwnedMutation(refreshedTarget)
            }
        } catch (error) {
            console.error(error)
            alertError(error)
        }
        $doingChat = false
        if(DBState.db.playMessage){
            const audio = new Audio(sendSound);
            audio.play().catch(() => {});
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

    function openScreenshotDialog() {
        screenshotError = ''
        screenshotCompletedTurns = 0
        const source = currentCharacter
        const chat = source?.chats[source.chatPage]
        screenshotDialogSnapshot = source && chat
            ? createChatScreenshotDialogSnapshot({
                characterId: source.chaId,
                chatId: chat.id ?? `${source.chaId}:${source.chatPage}`,
                messages: chat.message,
                renderContext: createScreenshotRenderContext(source, chat),
            })
            : null
        screenshotTotalTurns = screenshotDialogSnapshot?.totalTurns ?? 0
        screenshotDialogOpen = true
    }

    function cancelScreenshot() {
        screenshotController?.abort()
    }

    function closeScreenshotDialog() {
        cancelScreenshot()
        screenshotDialogOpen = false
        screenshotDialogSnapshot = null
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
        start: number,
        end: number,
    ) {
        const character = snapshotChatScreenshotCharacter(source, chat)
        const memberIds = new Set(source.type === 'group' ? source.characters : [])
        for (const message of chat.message.slice(Math.max(0, start - 2), end)) {
            if (message.saying) memberIds.add(message.saying)
        }
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
            parserContext: createCaptureParserContext(source, chat, 1, chat.message.length),
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
                swipe: DBState.db.swipe,
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

    async function startScreenshot(start: number, end: number) {
        const dialogSnapshot = screenshotDialogSnapshot
        if (screenshotRunning || !dialogSnapshot || !screenshotSurface) return

        const controller = new AbortController()
        screenshotController = controller
        screenshotRunning = true
        screenshotCompletedTurns = 0
        screenshotError = ''

        try {
            const job = createChatScreenshotJobFromDialogSnapshot(dialogSnapshot, start, end)
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
                        if (!canExportLongScreenshotArchive(isTauri)) {
                            throw new Error(language.screenshotLongNativeUnavailable)
                        }
                        const writer = new LocalWriter()
                        const selected = await writer.init('ZIP', ['zip'], `${fileBase}.zip`)
                        if (!selected) throw new DOMException('Screenshot export was cancelled', 'AbortError')
                        return createStreamingScreenshotArchive(writer)
                    },
                },
            })
            if (controller.signal.aborted) {
                throw new DOMException('Screenshot capture was cancelled', 'AbortError')
            }
            alertNormal(language.screenshotSaved)
            screenshotDialogOpen = false
            screenshotDialogSnapshot = null
        } catch (error) {
            if (!(error instanceof DOMException && error.name === 'AbortError')) {
                console.error(error)
                const detail = error instanceof Error ? error.message : String(error)
                screenshotError = language.screenshotFailed.replace('{error}', detail)
                alertError(screenshotError)
            }
        } finally {
            if (screenshotController === controller) screenshotController = null
            screenshotRunning = false
        }
    }

    onDestroy(cancelScreenshot)

    
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
            //@ts-expect-error scrollHeight/clientHeight/scrollTop don't exist on EventTarget, but target is HTMLElement here
            const scrolled = (e.target.scrollHeight - e.target.clientHeight + e.target.scrollTop)
            if(scrolled < 100 && DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length > loadPages){
                loadPages += getAdditionalChatLoadPages(DBState.db)
            }
            const chatTarget = e.target as HTMLElement;
            const chatsContainer = (DBState.db.fixedChatTextarea && chatTarget.children[1]) ? chatTarget.children[1] : chatTarget.children[0];
            const lastEl = chatsContainer?.firstElementChild;
            const isAtBottom = lastEl ? lastEl.getBoundingClientRect().top <= chatTarget.getBoundingClientRect().bottom + 100 : true;
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
                    <div onclick={(e) => {
                        const character = DBState.db.characters[$selectedCharID]
                        const chat = character.chats[character.chatPage]
                        const message = {
                            role: 'char',
                            data: ''
                        } as Message
                        const target = captureConversationMutationTarget(
                            character,
                            chat,
                            getActiveConversationSession(),
                        )
                        appendConversationMessage(target, message)
                        if (!target.session) character.chats[character.chatPage] = chat
                    }}
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
                            if(e.key.toLocaleLowerCase() === "enter" && (!e.shiftKey)){
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
                <Suggestion messageInput={(msg)=>messageInput=(
                    (DBState.db.subModel === "textgen_webui" || DBState.db.subModel === "mancer" || DBState.db.subModel.startsWith('local_')) && DBState.db.autoSuggestClean
                    ? msg.replace(/ +\(.+?\) *$| - [^"'*]*?$/, '')
                    : msg
                )} {send}/>
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

            {#if DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message?.[0]?.data?.startsWith(coldStorageHeader)  }
                {#await preLoadChat($selectedCharID, DBState.db.characters[$selectedCharID].chatPage)}
                    <div class="w-full flex justify-center text-textcolor2 italic mb-12">
                        {language.loadingChatData}
                    </div>
                {:then a}
                    <div></div>
                {/await}
            {:else}

            {#if chatFoldedStateMessageIndex.index !== -1}
                <button class="w-full flex justify-center max-w-full p-4">
                    <Button className="max-w-xl w-full" onclick={() => {
                        loadPages += chatFoldedStateMessageIndex.index + 1
                        chatFoldedState.data = null
                    }}>
                        {language.loadMore}
                    </Button>
                </button>
            {/if}
            
            <Chats
                bind:this={chatsInstance}
                messages={currentChat}
                loadPages={loadPages}
                onReroll={reroll}
                unReroll={unReroll}
                currentCharacter={currentCharacter}
                currentUsername={currentUsername}
                userIcon={userIcon}
                userIconPortrait={userIconPortrait}
                bind:hasNewUnreadMessage={showNewMessageButton}
            />

            {#if DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length <= loadPages}
                {#if DBState.db.characters[$selectedCharID].type !== 'group' }
                    <Chat
                        character={createSimpleCharacter(DBState.db.characters[$selectedCharID])}
                        name={DBState.db.characters[$selectedCharID].name}
                        message={DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].fmIndex === -1 ? DBState.db.characters[$selectedCharID].firstMessage :
                            DBState.db.characters[$selectedCharID].alternateGreetings[DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].fmIndex]}
                        role='char'
                        img={getCharImage(DBState.db.characters[$selectedCharID].image, 'css')}
                        idx={-1}
                        altGreeting={DBState.db.characters[$selectedCharID].alternateGreetings.length > 0}
                        largePortrait={DBState.db.characters[$selectedCharID].largePortrait}
                        firstMessage={true}
                        onReroll={() => {
                            const cha = DBState.db.characters[$selectedCharID]
                            const chat = DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage]
                            if(cha.type !== 'group'){
                                if (chat.fmIndex >= (cha.alternateGreetings.length - 1)){
                                    chat.fmIndex = -1
                                }
                                else{
                                    chat.fmIndex += 1
                                }
                            }
                            DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage] = chat
                        }}
                        unReroll={() => {
                            const cha = DBState.db.characters[$selectedCharID]
                            const chat = DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage]
                            if(cha.type !== 'group'){
                                if (chat.fmIndex === -1){
                                    chat.fmIndex = (cha.alternateGreetings.length - 1)
                                }
                                else{
                                    chat.fmIndex -= 1
                                }
                            }
                            DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage] = chat
                        }}
                        isLastMemory={false}
                        currentPage={(DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].fmIndex ?? -1) + 2}
                        totalPages={DBState.db.characters[$selectedCharID].alternateGreetings.length + 1}

                    />
                    {#if (aiLawApplies() && DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length === 0)}
                        <div class="ml-auto mr-auto mt-4 text-textcolor2 italic max-w-2/3 wrap-break-word text-center">
                            {language.aiGenerationWarning}
                        </div>
                    {/if}
                    {#if !DBState.db.characters[$selectedCharID].removedQuotes && DBState.db.characters[$selectedCharID].creatorNotes.length >= 2}
                        <CreatorQuote quote={DBState.db.characters[$selectedCharID].creatorNotes} onRemove={() => {
                            const cha = DBState.db.characters[$selectedCharID]
                            if(cha.type !== 'group'){
                                cha.removedQuotes = true
                            }
                            DBState.db.characters[$selectedCharID] = cha
                        }} />
                    {/if}
                {/if}
            {/if}

            {/if}

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
                        class:text-textcolor2={(DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length < 2) || (DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message[DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length - 1].role !== 'char')}
                        onclick={() => {
                            if((DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length < 2) || (DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message[DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].message.length - 1].role !== 'char')){
                                return
                            }
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
