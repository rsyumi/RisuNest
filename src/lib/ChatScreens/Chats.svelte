<script lang="ts">
    import type { character, groupChat, Message, StreamingDisplayOptimizationMode } from 'src/ts/storage/database.svelte';
    import { mount, onDestroy, unmount } from 'svelte';
    import Chat from './Chat.svelte';
    import { getCharImage } from 'src/ts/characters';
    import { createSimpleCharacter, DBState, selectedCharID, ReloadChatPointer, ReloadGUIPointer } from 'src/ts/stores.svelte';
    import { chatFoldedStateMessageIndex } from 'src/ts/globalApi.svelte';
    import { shouldContainChatMessage } from 'src/ts/chatLoadPages';
    import {
        areChatRenderSignaturesEqual,
        ChatRenderIdentityRegistry,
        createChatParserDependencyStamp,
        createChatRenderSignature,
        type ChatRenderSignature,
    } from 'src/ts/chatRenderIdentity';
    import { get } from 'svelte/store';

    let {
        messages,
        currentCharacter,
        onReroll,
        unReroll,
        currentUsername,
        userIcon,
        loadPages,
        userIconPortrait,
        hasNewUnreadMessage = $bindable(false)
    }:{
        messages: Message[]
        currentCharacter: character|groupChat
        onReroll: () => void
        unReroll: () => void
        currentUsername: string
        userIcon: string
        loadPages: number
        userIconPortrait?: boolean
        hasNewUnreadMessage?: boolean
    } = $props();

    let chatBody: HTMLDivElement;
    let renderKeys: Set<string> = new Set();
    const identityRegistry = new ChatRenderIdentityRegistry();
    type ChatInstance = {
        updateStreamingDisplay?: (state: {
            isOptimizedStreamingMessage: boolean
            streamingOptimizationMode: StreamingDisplayOptimizationMode
            rawStreamingText: string
        }) => void
    }
    let mountInstances: Map<string, ChatInstance> = new Map();
    let mountedElements: Map<string, HTMLElement> = new Map();
    let renderSignatures: Map<string, ChatRenderSignature> = new Map();
    let resolvedCharacterImage = $state<string | null>(null);
    let resolvedUserImage = $state<string | null>(null);
    let imagesReady = $state(false);
    let imageResolutionGeneration = 0;
    let hasRenderedChat = false;
    let simpleChar = $derived(createSimpleCharacter(currentCharacter));
    let parserCharacter = $derived(simpleChar ? {
        chaId: simpleChar.chaId,
        virtualscript: simpleChar.virtualscript,
        customscript: simpleChar.customscript,
        additionalAssets: simpleChar.additionalAssets,
        emotionImages: simpleChar.emotionImages,
        triggerscript: simpleChar.triggerscript,
    } : null);
    let parserCharacterStamp = $derived(createChatParserDependencyStamp(parserCharacter));

    $effect(() => {
        const characterImageSource = currentCharacter.image;
        const userImageSource = userIcon;
        void $ReloadGUIPointer;
        const generation = ++imageResolutionGeneration;
        imagesReady = false;
        resolvedCharacterImage = null;
        resolvedUserImage = null;
        void Promise.allSettled([
            getCharImage(characterImageSource, 'css'),
            getCharImage(userImageSource, 'css'),
        ]).then(([characterImage, resolvedUser]) => {
            if (generation !== imageResolutionGeneration) return;
            resolvedCharacterImage = characterImage.status === 'fulfilled' ? characterImage.value : null;
            resolvedUserImage = resolvedUser.status === 'fulfilled' ? resolvedUser.value : null;
            imagesReady = true;
        });
    });

    const updateChatBody = () => {
        if(!chatBody){
            return
        }

        let currentRenderKeys: Set<string> = new Set();
        let previousElement: HTMLElement | null = null;
        let loadStart = messages.length - 1
        let loadEnd = messages.length - loadPages
        const currentChat = currentCharacter.chats?.[currentCharacter.chatPage]
        const configuredPerformanceMode = DBState.db.streamingDisplayOptimizationMode ?? 'off';
        const performanceMode = currentChat?.isStreaming
            ? currentChat.activeStreamingDisplayOptimizationMode ?? configuredPerformanceMode
            : configuredPerformanceMode
        const activeStreamingIndex = performanceMode !== 'off' && currentChat?.isStreaming
            ? messages.length - 1
            : -1
        const selectedCharacterIndex = get(selectedCharID);
        const characterId = currentCharacter.type === 'group'
            ? `group:${selectedCharacterIndex}`
            : currentCharacter.chaId;
        const currentChatScope = currentChat?.id
            ?? `${selectedCharacterIndex}:${characterId}:${currentCharacter.chatPage}`
        const messageRenderKeys = identityRegistry.resolve(currentChatScope, messages)
        const globalReloadPointer = get(ReloadGUIPointer);
        if(chatFoldedStateMessageIndex.index !== -1){
            loadStart = chatFoldedStateMessageIndex.index
            loadEnd = Math.max(0, chatFoldedStateMessageIndex.index - loadPages)
        }

        const reloadPointerMap = get(ReloadChatPointer);

        for(let i=loadStart ; i >= loadEnd; i--){
            if(i < 0) break; // Prevent out of bounds
            const message = messages[i];
            const messageLargePortrait = message.role === 'user' ? (userIconPortrait ?? false) : ((currentCharacter as character).largePortrait ?? false);
            const reloadPointer = reloadPointerMap[i] ?? 0;
            const activeStreamingMessage = i === activeStreamingIndex && message.role === 'char';
            const currentRenderKey = messageRenderKeys[i];
            const renderSignature = createChatRenderSignature({
                message,
                index: i,
                totalLength: messages.length,
                largePortrait: messageLargePortrait,
                reloadPointer,
                globalReloadPointer,
                activeStreamingMessage,
                resolvedImage: message.role === 'user' ? resolvedUserImage : resolvedCharacterImage,
                displayName: message.role === 'user' ? currentUsername : currentCharacter.name,
                parserCharacter,
                parserCharacterStamp,
            });
            currentRenderKeys.add(currentRenderKey);
            const containInput = {
                index: i,
                totalLength: messages.length,
                isStreaming: currentChat?.isStreaming === true && i === messages.length - 1,
                isComment: message.isComment ?? false,
                data: message.data,
                captureAll: loadPages === Infinity,
            };
            const containMessage = shouldContainChatMessage(containInput);
            let element = mountedElements.get(currentRenderKey);
            if(!element){
                element = document.createElement('div');
                element.setAttribute('data-chat-render-key', currentRenderKey);
                element.classList.add('chat-message-container');
                mountedElements.set(currentRenderKey, element);
            }
            element.classList.toggle('is-settled-history', containMessage);

            const previousSignature = renderSignatures.get(currentRenderKey);
            if(!areChatRenderSignaturesEqual(previousSignature, renderSignature)){
                const previousInstance = mountInstances.get(currentRenderKey);
                if(previousInstance){
                    unmount(previousInstance);
                }
                element.replaceChildren();
                const inst = mount(Chat, {
                    target: element,
                    props: {
                        message: message.data,
                        isLastMemory: false,
                        idx: i,
                        totalLength: messages.length,
                        img: (message.role === 'user' ? resolvedUserImage : resolvedCharacterImage) ?? '',
                        onReroll: onReroll,
                        unReroll: unReroll,
                        rerollIcon: 'dynamic',
                        character: simpleChar,
                        largePortrait: message.role === 'user' ? (userIconPortrait ?? false) : ((currentCharacter as character).largePortrait ?? false),
                        messageGenerationInfo: message.generationInfo,
                        role: message.role,
                        name: message.role === 'user' ? currentUsername : currentCharacter.name,
                        isComment: message.isComment ?? false,
                        disabled: message.disabled ?? false,
                        isOptimizedStreamingMessage: activeStreamingMessage,
                        streamingOptimizationMode: performanceMode,
                        rawStreamingText: message.data,
                    },

                })
                mountInstances.set(currentRenderKey, inst);
                renderSignatures.set(currentRenderKey, renderSignature);
            }
            else{
                mountInstances.get(currentRenderKey)?.updateStreamingDisplay?.({
                    isOptimizedStreamingMessage: activeStreamingMessage,
                    streamingOptimizationMode: performanceMode,
                    rawStreamingText: message.data,
                })
            }
            if(previousElement){
                if(previousElement.nextElementSibling !== element){
                    previousElement.after(element);
                }
            }
            else if(chatBody.firstElementChild !== element){
                chatBody.prepend(element);
            }
            previousElement = element;
        }

        for(const renderKey of renderKeys){
            if(currentRenderKeys.has(renderKey)){
                continue;
            }
            const inst = mountInstances.get(renderKey);
            if(inst){
                unmount(inst);
                mountInstances.delete(renderKey);
            }
            const element = mountedElements.get(renderKey);
            if(element){
                element.remove();
                mountedElements.delete(renderKey);
            }
            renderSignatures.delete(renderKey);
        }

        renderKeys = currentRenderKeys;
        hasRenderedChat = true;
        
    };

    onDestroy(() => {
        console.log('Unmounting Chats');
        renderKeys.clear();
        mountInstances.forEach((inst) => {
            unmount(inst);
        });
        mountInstances.clear();
        mountedElements.clear();
        renderSignatures.clear();
        imageResolutionGeneration++;
    })

    function checkIfAtBottom() {
        if (!chatBody || !chatBody.parentElement) return true;
        const sc = chatBody.parentElement;
        const lastEl = chatBody.firstElementChild;
        if (!lastEl) return true;
        const rect = lastEl.getBoundingClientRect();
        const scRect = sc.getBoundingClientRect();
        return rect.top <= scRect.bottom + 100;
    }

    export const scrollToLatestMessage = () => {
        if(!chatBody) return;
        hasNewUnreadMessage = false;
        const element = chatBody.firstElementChild;
        if(element){
             element.scrollIntoView({ behavior: 'instant', block: 'start' });
        }
    }

    let previousLength = 0;
    let previousChatRoomId: string | null = null;

    $effect(() => {
        console.log('Updating Chats');
        void $ReloadChatPointer; // Make $effect track ReloadChatPointer changes
        if (!imagesReady && !hasRenderedChat) return;
        const wasAtBottom = checkIfAtBottom();
        updateChatBody()
        
        const currentChatRoomId = currentCharacter.chats?.[currentCharacter.chatPage]?.id
            ?? `${get(selectedCharID)}:${currentCharacter.chatPage}`;
        const isSameChat = currentChatRoomId === previousChatRoomId;
        
        // Only auto-scroll if it's the same chat and new messages were added
        if(isSameChat && messages.length > previousLength){
            const lastMsg = messages[messages.length - 1];
            if(lastMsg && lastMsg.role === 'char' && DBState.db.autoScrollToNewMessage){
                if(wasAtBottom || DBState.db.alwaysScrollToNewMessage){
                    const element = chatBody.firstElementChild;
                    if(element){
                        setTimeout(() => {
                            element.scrollIntoView({ behavior: 'instant', block: 'start' });
                        }, 700);
                    }
                } else {
                    hasNewUnreadMessage = true;
                }
            }
        }
        previousLength = messages.length;
        previousChatRoomId = currentChatRoomId;
    })

</script>

<div class="flex flex-col-reverse" bind:this={chatBody}></div>
