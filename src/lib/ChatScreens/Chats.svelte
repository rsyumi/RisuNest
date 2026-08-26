<script lang="ts">
    import type { character, groupChat, Message, StreamingDisplayOptimizationMode } from 'src/ts/storage/database.svelte'
    import { mount, onDestroy, onMount, tick, unmount } from 'svelte'
    import { get } from 'svelte/store'
    import Chat from './Chat.svelte'
    import ChatConversationStart from './ChatConversationStart.svelte'
    import { getCharImage } from 'src/ts/characters'
    import { createSimpleCharacter, DBState, selectedCharID, ReloadChatPointer, ReloadGUIPointer } from 'src/ts/stores.svelte'
    import {
        areChatRenderSignaturesEqual,
        ChatRenderIdentityRegistry,
        type ChatRenderIdentitySequence,
        createChatParserDependencyStamp,
        createChatRenderSignature,
        type ChatRenderSignature,
    } from 'src/ts/chatRenderIdentity'
    import {
        buildChatViewport,
        type ChatViewportAnchor,
        type ChatViewportJumpOptions,
        type ChatViewportPin,
        type ChatViewportPinReason,
        type ChatViewportResult,
    } from 'src/ts/chatViewport'
    import {
        getRuntimePerformanceBudgets,
        subscribeRuntimePerformanceProfile,
    } from 'src/ts/runtimePerformanceProfile'

    let {
        messages,
        currentCharacter,
        onReroll,
        unReroll,
        onFirstMessageReroll = () => {},
        unFirstMessageReroll = () => {},
        onRemoveCreatorQuote = () => {},
        showAiWarning = false,
        currentUsername,
        userIcon,
        userIconPortrait,
        hasNewUnreadMessage = $bindable(false),
    }: {
        messages: Message[]
        currentCharacter: character | groupChat
        onReroll: () => void
        unReroll: () => void
        onFirstMessageReroll?: () => void
        unFirstMessageReroll?: () => void
        onRemoveCreatorQuote?: () => void
        showAiWarning?: boolean
        currentUsername: string
        userIcon: string
        userIconPortrait?: boolean
        hasNewUnreadMessage?: boolean
    } = $props()

    const ESTIMATED_MESSAGE_HEIGHT = 256
    const VIEWPORT_OVERSCAN = 8

    type ChatInstance = {
        updateStreamingDisplay?: (state: {
            isOptimizedStreamingMessage: boolean
            streamingOptimizationMode: StreamingDisplayOptimizationMode
            rawStreamingText: string
        }) => void
    }

    let chatBody: HTMLDivElement
    let renderKeys = new Set<string>()
    let mountInstances = new Map<string, ChatInstance>()
    let mountedElements = new Map<string, HTMLElement>()
    let renderSignatures = new Map<string, ChatRenderSignature>()
    let measuredHeights = new Map<string, number>()
    let pinReasons = new Map<string, Set<ChatViewportPinReason>>()
    let playingMedia = new Map<string, Set<EventTarget>>()
    let viewportAnchor: ChatViewportAnchor | null = null
    let viewportResult: ChatViewportResult | null = null
    let identitySequence: ChatRenderIdentitySequence | null = null
    let messageRenderKeys: string[] = []
    let registeredScope: string | null = null
    let registeredMessages: Message[] | null = null
    let registeredLength = 0
    let previousReloadPointer: unknown = null
    let activeScope: string | null = null
    let resizeObserver: ResizeObserver | null = null
    let scrollContainer: HTMLElement | null = null
    let lastScrollTop = 0
    let scheduledReconcileFrame: number | null = null
    const animationFrames = new Set<number>()
    const layoutFrameResolvers = new Map<number, () => void>()
    let highlightTimer: ReturnType<typeof setTimeout> | null = null
    let autoScrollTimer: ReturnType<typeof setTimeout> | null = null
    let navigationGeneration = 0
    let suppressScroll = false
    const identityRegistry = new ChatRenderIdentityRegistry()
    const ownerSessionIds = new WeakMap<object, number>()
    const conversationSessionIds = new WeakMap<object, number>()
    let nextScopeIdentity = 1

    let resolvedCharacterImage = $state<string | null>(null)
    let resolvedUserImage = $state<string | null>(null)
    let imagesReady = $state(false)
    let imageResolutionGeneration = 0
    let hasRenderedChat = false
    let simpleChar = $derived(createSimpleCharacter(currentCharacter))
    let parserCharacter = $derived(simpleChar ? {
        chaId: simpleChar.chaId,
        virtualscript: simpleChar.virtualscript,
        customscript: simpleChar.customscript,
        additionalAssets: simpleChar.additionalAssets,
        emotionImages: simpleChar.emotionImages,
        triggerscript: simpleChar.triggerscript,
    } : null)
    let parserCharacterStamp = $derived(createChatParserDependencyStamp(parserCharacter))

    $effect(() => {
        const characterImageSource = currentCharacter.image
        const userImageSource = userIcon
        void $ReloadGUIPointer
        const generation = ++imageResolutionGeneration
        imagesReady = false
        resolvedCharacterImage = null
        resolvedUserImage = null
        void Promise.allSettled([
            getCharImage(characterImageSource, 'css'),
            getCharImage(userImageSource, 'css'),
        ]).then(([characterImage, resolvedUser]) => {
            if (generation !== imageResolutionGeneration) return
            resolvedCharacterImage = characterImage.status === 'fulfilled' ? characterImage.value : null
            resolvedUserImage = resolvedUser.status === 'fulfilled' ? resolvedUser.value : null
            imagesReady = true
        })
    })

    function currentChatScope(): string {
        const currentChat = currentCharacter.chats?.[currentCharacter.chatPage]
        const selectedCharacterIndex = get(selectedCharID)
        const ownerId = currentCharacter.type === 'group'
            ? `group:${selectedCharacterIndex}`
            : `character:${selectedCharacterIndex}:${currentCharacter.chaId}`
        const ownerSessionId = objectScopeIdentity(ownerSessionIds, currentCharacter)
        const conversationId = currentChat?.id ?? `page:${currentCharacter.chatPage}`
        const conversationSessionId = currentChat
            ? objectScopeIdentity(conversationSessionIds, currentChat)
            : 0
        return [
            ownerId,
            String(ownerSessionId),
            conversationId,
            String(conversationSessionId),
        ].map((part) => `${part.length}:${part}`).join('|')
    }

    function objectScopeIdentity(identities: WeakMap<object, number>, value: object): number {
        const existing = identities.get(value)
        if (existing !== undefined) return existing
        const identity = nextScopeIdentity++
        identities.set(value, identity)
        return identity
    }

    function hasConversationStart(): boolean {
        return currentCharacter.type !== 'group'
    }

    function conversationStartKey(scope: string): string {
        return `${scope.length}:${scope}|conversation-start`
    }

    function viewportKeys(scope: string): string[] {
        return hasConversationStart()
            ? [conversationStartKey(scope), ...messageRenderKeys]
            : messageRenderKeys
    }

    function resetViewport(scope: string): void {
        if (activeScope !== null) navigationGeneration += 1
        clearScheduledWork()
        clearMountedRows()
        measuredHeights = new Map()
        pinReasons = new Map()
        playingMedia = new Map()
        viewportAnchor = null
        viewportResult = null
        identitySequence = null
        messageRenderKeys = []
        registeredScope = null
        registeredMessages = null
        registeredLength = 0
        previousReloadPointer = null
        activeScope = scope
    }

    function syncIdentityRegistration(scope: string, reloadPointer: unknown): void {
        const needsStructuralRegistration = (
            registeredScope !== scope
            || registeredMessages !== messages
            || messages.length < registeredLength
            || reloadPointer !== previousReloadPointer
        )
        if (needsStructuralRegistration) {
            identitySequence = identityRegistry.register(scope, messages)
        } else if (messages.length > registeredLength) {
            identitySequence = identityRegistry.registerAppend(scope, messages, registeredLength)
        } else if (!identitySequence) {
            identitySequence = identityRegistry.register(scope, messages)
        }
        if (needsStructuralRegistration || messages.length !== registeredLength || messageRenderKeys.length === 0) {
            messageRenderKeys = identitySequence.toArray()
        }
        registeredScope = scope
        registeredMessages = messages
        registeredLength = messages.length
        previousReloadPointer = reloadPointer
    }

    function currentPins(currentChat: character['chats'][number] | groupChat['chats'][number] | undefined): ChatViewportPin[] {
        const pins: ChatViewportPin[] = []
        for (const [key, reasons] of pinReasons) {
            for (const reason of reasons) pins.push({ key, reason })
        }
        if (currentChat?.isStreaming && messageRenderKeys.length > 0) {
            pins.push({ key: messageRenderKeys.at(-1)!, reason: 'streaming' })
        }
        return pins
    }

    function captureDomAnchor(): ChatViewportAnchor | null {
        if (!scrollContainer) return viewportAnchor
        const containerRect = scrollContainer.getBoundingClientRect()
        if (containerRect.height <= 0) return viewportAnchor
        let closest: { key: string; index: number; offset: number; distance: number } | null = null
        for (const [key, element] of mountedElements) {
            const indexText = element.dataset.chatViewportIndex
            if (indexText === undefined) continue
            const index = Number(indexText)
            const rect = element.getBoundingClientRect()
            if (rect.bottom < containerRect.top || rect.top > containerRect.bottom) continue
            const offset = rect.top - containerRect.top
            const distance = Math.abs(offset)
            if (!closest || distance < closest.distance) closest = { key, index, offset, distance }
        }
        return closest
            ? { key: closest.key, indexHint: closest.index, relativeOffset: closest.offset }
            : viewportAnchor
    }

    function correctDomAnchor(anchor: ChatViewportAnchor | null): void {
        if (!anchor || !scrollContainer) return
        const element = mountedElements.get(anchor.key)
        if (!element) return
        const currentOffset = element.getBoundingClientRect().top
            - scrollContainer.getBoundingClientRect().top
        const delta = currentOffset - anchor.relativeOffset
        if (Math.abs(delta) < 0.5 || typeof scrollContainer.scrollBy !== 'function') return
        suppressScroll = true
        scrollContainer.scrollBy({ top: delta, behavior: 'instant' })
        scheduleFrame(() => {
            suppressScroll = false
            if (scrollContainer) lastScrollTop = scrollContainer.scrollTop
        })
    }

    function reconcileViewport(options: {
        jumpTarget?: number
        preserveAnchor?: boolean
        anchor?: ChatViewportAnchor | null
    } = {}): ChatViewportResult | null {
        if (!chatBody) return null
        const scope = currentChatScope()
        if (activeScope !== scope) resetViewport(scope)
        const reloadPointerMap = get(ReloadChatPointer)
        syncIdentityRegistration(scope, reloadPointerMap)
        const keys = viewportKeys(scope)
        const preservedAnchor = options.anchor !== undefined
            ? options.anchor
            : options.preserveAnchor === false ? viewportAnchor : captureDomAnchor()
        const currentChat = currentCharacter.chats?.[currentCharacter.chatPage]
        const budget = getRuntimePerformanceBudgets().chatMountedMessageBudget
        const result = buildChatViewport({
            keys,
            budget,
            overscan: Math.min(VIEWPORT_OVERSCAN, budget - 1),
            estimatedMessageHeight: ESTIMATED_MESSAGE_HEIGHT,
            measuredHeights,
            anchor: preservedAnchor,
            jumpTarget: options.jumpTarget,
            pins: currentPins(currentChat),
        })
        viewportAnchor = result.anchor
        viewportResult = result
        renderViewportRows(scope, result, currentChat, reloadPointerMap)
        chatBody.dataset.chatPinOverflow = String(result.pinOverflow?.count ?? 0)
        correctDomAnchor(preservedAnchor)
        hasRenderedChat = true
        return result
    }

    function renderViewportRows(
        scope: string,
        result: ChatViewportResult,
        currentChat: character['chats'][number] | groupChat['chats'][number] | undefined,
        reloadPointerMap: Record<number, number>,
    ): void {
        const currentRenderKeys = new Set<string>()
        const orderedElements: HTMLElement[] = []
        const configuredPerformanceMode = DBState.db.streamingDisplayOptimizationMode ?? 'off'
        const performanceMode = currentChat?.isStreaming
            ? currentChat.activeStreamingDisplayOptimizationMode ?? configuredPerformanceMode
            : configuredPerformanceMode
        const activeStreamingIndex = performanceMode !== 'off' && currentChat?.isStreaming
            ? messages.length - 1
            : -1
        const globalReloadPointer = get(ReloadGUIPointer)
        const startOffset = hasConversationStart() ? 1 : 0

        for (const row of [...result.rows].reverse()) {
            if (row.kind === 'gap') {
                const gap = document.createElement('div')
                gap.className = 'chat-viewport-gap'
                gap.dataset.chatGap = `${row.startIndex}:${row.endIndex}`
                gap.dataset.chatGapStart = String(row.startIndex)
                gap.dataset.chatGapEnd = String(row.endIndex)
                gap.setAttribute('aria-hidden', 'true')
                gap.style.height = `${row.height}px`
                gap.style.flexBasis = `${row.height}px`
                orderedElements.push(gap)
                continue
            }

            if (startOffset === 1 && row.index === 0) {
                const key = conversationStartKey(scope)
                const element = ensureElement(key, row.index)
                currentRenderKeys.add(key)
                renderConversationStart(key, element)
                element.classList.remove('is-latest-chat-row', 'is-settled-history')
                orderedElements.push(element)
                continue
            }

            const index = row.index - startOffset
            const message = messages[index]
            const key = row.key
            if (!message) continue
            const element = ensureElement(key, row.index)
            element.dataset.chatIndex = String(index)
            currentRenderKeys.add(key)
            const messageLargePortrait = message.role === 'user'
                ? (userIconPortrait ?? false)
                : ((currentCharacter as character).largePortrait ?? false)
            const activeStreamingMessage = index === activeStreamingIndex && message.role === 'char'
            const renderSignature = createChatRenderSignature({
                message,
                index,
                totalLength: messages.length,
                largePortrait: messageLargePortrait,
                reloadPointer: reloadPointerMap[index] ?? 0,
                globalReloadPointer,
                activeStreamingMessage,
                resolvedImage: message.role === 'user' ? resolvedUserImage : resolvedCharacterImage,
                displayName: message.role === 'user' ? currentUsername : currentCharacter.name,
                parserCharacter,
                parserCharacterStamp,
            })
            const previousSignature = renderSignatures.get(key)
            if (!areChatRenderSignaturesEqual(previousSignature, renderSignature)) {
                releaseRowRuntimeState(key)
                unmountInstance(key)
                element.replaceChildren()
                const instance = mount(Chat, {
                    target: element,
                    props: {
                        message: message.data,
                        isLastMemory: false,
                        idx: index,
                        totalLength: messages.length,
                        img: (message.role === 'user' ? resolvedUserImage : resolvedCharacterImage) ?? '',
                        onReroll,
                        unReroll,
                        rerollIcon: 'dynamic',
                        character: simpleChar,
                        largePortrait: messageLargePortrait,
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
                mountInstances.set(key, instance)
                renderSignatures.set(key, renderSignature)
            } else {
                mountInstances.get(key)?.updateStreamingDisplay?.({
                    isOptimizedStreamingMessage: activeStreamingMessage,
                    streamingOptimizationMode: performanceMode,
                    rawStreamingText: message.data,
                })
            }
            const latest = index === messages.length - 1
            element.classList.toggle('is-latest-chat-row', latest)
            element.classList.toggle(
                'is-settled-history',
                !latest && !activeStreamingMessage && row.pinReasons.length === 0,
            )
            orderedElements.push(element)
        }

        for (const key of renderKeys) {
            if (!currentRenderKeys.has(key)) removeMountedRow(key)
        }
        reconcileChatBodyChildren(orderedElements)
        renderKeys = currentRenderKeys
    }

    function reconcileChatBodyChildren(orderedElements: readonly HTMLElement[]): void {
        const retained = new Set<Node>(orderedElements)
        for (const child of [...chatBody.childNodes]) {
            if (!retained.has(child)) child.remove()
        }

        let cursor = chatBody.firstChild
        for (const element of orderedElements) {
            if (cursor === element) {
                cursor = cursor.nextSibling
                continue
            }
            chatBody.insertBefore(element, cursor)
        }
    }

    function ensureElement(key: string, viewportIndex: number): HTMLElement {
        let element = mountedElements.get(key)
        if (!element) {
            element = document.createElement('div')
            element.dataset.chatRenderKey = key
            element.classList.add('chat-message-container')
            mountedElements.set(key, element)
            resizeObserver?.observe(element)
        }
        element.dataset.chatViewportIndex = String(viewportIndex)
        return element
    }

    function renderConversationStart(key: string, element: HTMLElement): void {
        const character = currentCharacter as character
        const currentChat = character.chats[character.chatPage]
        const signature = JSON.stringify([
            character.chaId,
            currentChat.id,
            currentChat.fmIndex,
            character.firstMessage,
            character.alternateGreetings,
            character.creatorNotes,
            character.removedQuotes,
            character.largePortrait,
            resolvedCharacterImage,
            showAiWarning,
        ])
        if (element.dataset.chatConversationStartSignature === signature && mountInstances.has(key)) return
        unmountInstance(key)
        element.replaceChildren()
        const instance = mount(ChatConversationStart, {
            target: element,
            props: {
                currentCharacter: character,
                resolvedImage: resolvedCharacterImage ?? '',
                showAiWarning,
                onReroll: onFirstMessageReroll,
                unReroll: unFirstMessageReroll,
                onRemoveCreatorQuote,
            },
        })
        mountInstances.set(key, instance)
        element.dataset.chatConversationStartSignature = signature
    }

    function unmountInstance(key: string): void {
        const instance = mountInstances.get(key)
        if (!instance) return
        unmount(instance)
        mountInstances.delete(key)
    }

    function removeMountedRow(key: string): void {
        releaseRowRuntimeState(key)
        unmountInstance(key)
        const element = mountedElements.get(key)
        if (element) {
            resizeObserver?.unobserve(element)
            element.remove()
            mountedElements.delete(key)
        }
        renderSignatures.delete(key)
    }

    function releaseRowRuntimeState(key: string): void {
        const media = playingMedia.get(key)
        playingMedia.delete(key)
        pinReasons.delete(key)
        for (const target of media ?? []) {
            if (!(target instanceof HTMLMediaElement)) continue
            try {
                target.pause()
            } catch {
                // The component teardown below remains the resource owner.
            }
        }
    }

    function clearMountedRows(): void {
        for (const key of [...renderKeys]) removeMountedRow(key)
        for (const key of [...mountedElements.keys()]) removeMountedRow(key)
        renderKeys.clear()
        chatBody?.replaceChildren()
    }

    function addPin(key: string, reason: ChatViewportPinReason): void {
        const reasons = pinReasons.get(key) ?? new Set<ChatViewportPinReason>()
        if (reasons.has(reason)) return
        reasons.add(reason)
        pinReasons.set(key, reasons)
        reconcileViewport()
    }

    function removePin(key: string, reason: ChatViewportPinReason): void {
        const reasons = pinReasons.get(key)
        if (!reasons?.delete(reason)) return
        if (reasons.size === 0) pinReasons.delete(key)
        reconcileViewport()
    }

    function rowKeyFromEvent(event: Event): string | null {
        const target = event.target
        if (!(target instanceof Element)) return null
        return target.closest<HTMLElement>('[data-chat-render-key]')?.dataset.chatRenderKey ?? null
    }

    function handleFocusIn(event: FocusEvent): void {
        const key = rowKeyFromEvent(event)
        if (key) addPin(key, 'editor')
    }

    function handleFocusOut(event: FocusEvent): void {
        const key = rowKeyFromEvent(event)
        if (!key) return
        const row = mountedElements.get(key)
        if (event.relatedTarget instanceof Node && row?.contains(event.relatedTarget)) return
        queueMicrotask(() => {
            if (row && document.activeElement instanceof Node && row.contains(document.activeElement)) return
            removePin(key, 'editor')
        })
    }

    function handleMediaPlay(event: Event): void {
        const key = rowKeyFromEvent(event)
        if (!key || !event.target) return
        const media = playingMedia.get(key) ?? new Set<EventTarget>()
        media.add(event.target)
        playingMedia.set(key, media)
        addPin(key, 'playing-media')
    }

    function handleMediaStop(event: Event): void {
        const key = rowKeyFromEvent(event)
        if (!key || !event.target) return
        const media = playingMedia.get(key)
        media?.delete(event.target)
        if (media && media.size > 0) return
        playingMedia.delete(key)
        removePin(key, 'playing-media')
    }

    function scheduleFrame(callback: () => void): number | null {
        if (typeof requestAnimationFrame === 'function') {
            const frame = requestAnimationFrame(() => {
                animationFrames.delete(frame)
                callback()
            })
            animationFrames.add(frame)
            return frame
        }
        queueMicrotask(callback)
        return null
    }

    function clearScheduledWork(): void {
        if (typeof cancelAnimationFrame === 'function') {
            for (const frame of animationFrames) cancelAnimationFrame(frame)
        }
        animationFrames.clear()
        const pendingLayoutResolves = [...layoutFrameResolvers.values()]
        layoutFrameResolvers.clear()
        for (const resolve of pendingLayoutResolves) resolve()
        scheduledReconcileFrame = null
        if (highlightTimer) clearTimeout(highlightTimer)
        highlightTimer = null
        if (autoScrollTimer) clearTimeout(autoScrollTimer)
        autoScrollTimer = null
    }

    function handleResize(entries: ResizeObserverEntry[]): void {
        const anchor = captureDomAnchor()
        let changed = false
        for (const entry of entries) {
            const element = entry.target as HTMLElement
            const key = element.dataset.chatRenderKey
            const height = element.getBoundingClientRect().height || entry.contentRect.height
            if (!key || !Number.isFinite(height) || height <= 0) continue
            if (Math.abs((measuredHeights.get(key) ?? 0) - height) < 0.5) continue
            measuredHeights.set(key, height)
            changed = true
        }
        if (!changed) return
        viewportAnchor = anchor
        if (scheduledReconcileFrame !== null) return
        scheduledReconcileFrame = scheduleFrame(() => {
            scheduledReconcileFrame = null
            reconcileViewport({ anchor })
        })
    }

    function handleScroll(): void {
        if (!scrollContainer || suppressScroll || !viewportResult) return
        const currentTop = scrollContainer.scrollTop
        const movingOlder = currentTop < lastScrollTop
        const movingNewer = currentTop > lastScrollTop
        lastScrollTop = currentTop
        if (!movingOlder && !movingNewer) return
        const containerRect = scrollContainer.getBoundingClientRect()
        const gaps = [...chatBody.querySelectorAll<HTMLElement>('[data-chat-gap]')]
        const visibleGap = gaps.find((gap) => {
            const rect = gap.getBoundingClientRect()
            return rect.bottom >= containerRect.top && rect.top <= containerRect.bottom
        })
        if (!visibleGap) return
        const start = Number(visibleGap.dataset.chatGapStart)
        const end = Number(visibleGap.dataset.chatGapEnd)
        const target = movingOlder ? end - 1 : start
        const keys = viewportKeys(currentChatScope())
        if (!Number.isInteger(target) || target < 0 || target >= keys.length) return
        viewportAnchor = {
            key: keys[target],
            indexHint: target,
            relativeOffset: viewportAnchor?.relativeOffset ?? 0,
        }
        reconcileViewport({ jumpTarget: target })
    }

    function checkIfAtBottom(): boolean {
        if (!scrollContainer || messageRenderKeys.length === 0) return true
        const latest = mountedElements.get(messageRenderKeys.at(-1)!)
        if (!latest) return false
        return latest.getBoundingClientRect().top <= scrollContainer.getBoundingClientRect().bottom + 100
    }

    async function waitForLayout(): Promise<void> {
        await tick()
        await new Promise<void>((resolve) => {
            if (typeof requestAnimationFrame !== 'function') {
                queueMicrotask(resolve)
                return
            }
            const frame = requestAnimationFrame(() => {
                animationFrames.delete(frame)
                layoutFrameResolvers.delete(frame)
                resolve()
            })
            animationFrames.add(frame)
            layoutFrameResolvers.set(frame, resolve)
        })
    }

    export async function jumpTo(index: number, options: ChatViewportJumpOptions = {}): Promise<boolean> {
        if (!Number.isInteger(index) || index < 0 || index >= messages.length) return false
        const generation = ++navigationGeneration
        const scope = currentChatScope()
        const startOffset = hasConversationStart() ? 1 : 0
        const result = reconcileViewport({ jumpTarget: index + startOffset, preserveAnchor: false })
        if (!result?.jumpAccepted) return false
        const key = messageRenderKeys[index]
        await waitForLayout()
        if (generation !== navigationGeneration || scope !== currentChatScope()) return false
        const element = mountedElements.get(key)
        if (!element) return false
        suppressScroll = true
        element.scrollIntoView?.({ behavior: 'instant', block: options.align ?? 'start' })
        if (options.highlight) {
            if (highlightTimer) clearTimeout(highlightTimer)
            element.classList.add('ring-2', 'ring-blue-500')
            highlightTimer = setTimeout(() => {
                element.classList.remove('ring-2', 'ring-blue-500')
                highlightTimer = null
            }, 2000)
        }
        viewportAnchor = {
            key,
            indexHint: index + startOffset,
            relativeOffset: element.getBoundingClientRect().top
                - (scrollContainer?.getBoundingClientRect().top ?? 0),
        }
        suppressScroll = false
        if (scrollContainer) lastScrollTop = scrollContainer.scrollTop
        return true
    }

    export async function jumpToLatestMessage(): Promise<void> {
        hasNewUnreadMessage = false
        if (messages.length > 0) {
            await jumpTo(messages.length - 1)
            return
        }
        const scope = currentChatScope()
        if (!hasConversationStart()) return
        reconcileViewport({ jumpTarget: 0, preserveAnchor: false })
        await waitForLayout()
        mountedElements.get(conversationStartKey(scope))?.scrollIntoView?.({ behavior: 'instant', block: 'start' })
    }

    export async function scrollToLatestMessage(): Promise<void> {
        await jumpToLatestMessage()
    }

    let previousLength = 0
    let previousConversationScope: string | null = null

    $effect(() => {
        void $ReloadChatPointer
        if (!imagesReady && !hasRenderedChat) return
        const wasAtBottom = checkIfAtBottom()
        reconcileViewport()
        const conversationScope = currentChatScope()
        const isSameChat = conversationScope === previousConversationScope
        if (isSameChat && messages.length > previousLength) {
            const lastMessage = messages.at(-1)
            if (lastMessage?.role === 'char' && DBState.db.autoScrollToNewMessage) {
                if (wasAtBottom || DBState.db.alwaysScrollToNewMessage) {
                    if (autoScrollTimer) clearTimeout(autoScrollTimer)
                    autoScrollTimer = setTimeout(() => {
                        autoScrollTimer = null
                        void jumpToLatestMessage()
                    }, 700)
                } else {
                    hasNewUnreadMessage = true
                }
            }
        }
        previousLength = messages.length
        previousConversationScope = conversationScope
    })

    onMount(() => {
        scrollContainer = chatBody.parentElement
        lastScrollTop = scrollContainer?.scrollTop ?? 0
        if (typeof ResizeObserver !== 'undefined') {
            resizeObserver = new ResizeObserver(handleResize)
            for (const element of mountedElements.values()) resizeObserver.observe(element)
        }
        chatBody.addEventListener('focusin', handleFocusIn)
        chatBody.addEventListener('focusout', handleFocusOut)
        chatBody.addEventListener('play', handleMediaPlay, true)
        chatBody.addEventListener('pause', handleMediaStop, true)
        chatBody.addEventListener('ended', handleMediaStop, true)
        scrollContainer?.addEventListener('scroll', handleScroll)
        const unsubscribeProfile = subscribeRuntimePerformanceProfile(() => reconcileViewport())
        return () => {
            unsubscribeProfile()
            chatBody.removeEventListener('focusin', handleFocusIn)
            chatBody.removeEventListener('focusout', handleFocusOut)
            chatBody.removeEventListener('play', handleMediaPlay, true)
            chatBody.removeEventListener('pause', handleMediaStop, true)
            chatBody.removeEventListener('ended', handleMediaStop, true)
            scrollContainer?.removeEventListener('scroll', handleScroll)
            resizeObserver?.disconnect()
            resizeObserver = null
            scrollContainer = null
        }
    })

    onDestroy(() => {
        navigationGeneration += 1
        clearScheduledWork()
        clearMountedRows()
        renderSignatures.clear()
        measuredHeights.clear()
        pinReasons.clear()
        playingMedia.clear()
        imageResolutionGeneration += 1
    })

</script>

<div class="flex flex-col-reverse" bind:this={chatBody}></div>
