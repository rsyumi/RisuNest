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
        type ChatViewportKeySource,
        type ChatViewportPin,
        type ChatViewportPinReason,
        type ChatViewportResult,
    } from 'src/ts/chatViewport'
    import {
        getRuntimePerformanceBudgets,
        subscribeRuntimePerformanceProfile,
    } from 'src/ts/runtimePerformanceProfile'
    import type {
        ConversationViewportPin as ConversationSourcePin,
        ConversationViewportKey,
        ConversationViewportRow,
        ConversationViewportSnapshot,
        ConversationViewportSource,
    } from 'src/ts/conversationViewportSource'
    import type {
        BoundedLiveChatParserProjection,
        LiveChatParserProjection,
        LiveChatParserProjectionResolver,
    } from 'src/ts/selectedConversationLiveParserProjection'

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
        viewportSource = null,
        parserProjectionResolver,
        hasNewUnreadMessage = $bindable(false),
    }: {
        messages?: Message[]
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
        viewportSource?: ConversationViewportSource | null
        parserProjectionResolver?: LiveChatParserProjectionResolver
        hasNewUnreadMessage?: boolean
    } = $props()

    const ESTIMATED_MESSAGE_HEIGHT = 256
    const VIEWPORT_OVERSCAN = 8
    const MEASURED_HEIGHT_CACHE_LIMIT = 256
    const PARSER_PROJECTION_RETRY_DELAY_MS = 250

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
    let measuredHeightIndices = new Map<number, number>()
    let measuredHeightIndexByKey = new Map<string, number>()
    let measuredHeightRecency = new Map<string, number>()
    let measuredHeightClock = 0
    let keyLookupScans = 0
    let pinReasons = new Map<string, Set<ChatViewportPinReason>>()
    let playingMedia = new Map<string, Set<EventTarget>>()
    let sourcePins = new Map<string, ConversationSourcePin>()
    let sourceLoads = new Map<string, AbortController>()
    interface RowParserProjectionState {
        readonly controller: AbortController
        readonly navigationGeneration: number
        readonly source: ConversationViewportSource
        readonly sourceToken: string
        readonly sourceVersion: number
        readonly row: ConversationViewportRow
        readonly totalMessages: number
        readonly renderSignature: ChatRenderSignature
        projection: LiveChatParserProjection | null
        failed: boolean
        retryTimer: ReturnType<typeof setTimeout> | null
    }
    let rowParserProjections = new Map<string, RowParserProjectionState>()
    let sourceUnsubscribe: (() => void) | null = null
    let activeViewportSource: ConversationViewportSource | null = null
    let sourceUpdateRevision = $state(0)
    let lastMountedSourceTailKey: string | null = null
    let pendingSourceWasAtBottom: boolean | null = null
    let pendingSourceHandoffAnchor: ChatViewportAnchor | null = null
    let renderedConversationIdentity: string | null = null
    let viewportAnchor: ChatViewportAnchor | null = null
    let viewportResult: ChatViewportResult | null = null
    let identitySequence: ChatRenderIdentitySequence | null = null
    let messageRenderKeys: string[] = []
    let registeredScope: string | null = null
    let registeredMessages: Message[] | undefined
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

    function currentConversationIdentity(): string {
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

    function currentConversationHandoffIdentity(): string {
        const currentChat = currentCharacter.chats?.[currentCharacter.chatPage]
        const selectedCharacterIndex = get(selectedCharID)
        const ownerId = currentCharacter.type === 'group'
            ? `group:${selectedCharacterIndex}:${currentCharacter.chaId}`
            : `character:${currentCharacter.chaId}`
        const conversationId = currentChat?.id ?? `page:${currentCharacter.chatPage}`
        return [ownerId, conversationId]
            .map((part) => `${part.length}:${part}`)
            .join('|')
    }

    function currentChatScope(): string {
        const conversationIdentity = currentConversationIdentity()
        const sourceIdentity = activeViewportSource?.snapshot().sourceToken
            ?? 'compatibility-array'
        return `${conversationIdentity}|${sourceIdentity.length}:${sourceIdentity}`
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

    function viewportKeySource(
        scope: string,
        sourceSnapshot: ConversationViewportSnapshot | null = currentSourceSnapshot(),
    ): ChatViewportKeySource {
        const startOffset = hasConversationStart() ? 1 : 0
        const startKey = startOffset === 1 ? conversationStartKey(scope) : null
        if (sourceSnapshot) {
            return {
                length: sourceSnapshot.totalMessages + startOffset,
                keyAt(index) {
                    if (startKey !== null && index === 0) return startKey
                    return sourceSnapshot.keyAt(index - startOffset)
                },
                indexOf(key) {
                    if (startKey !== null && key === startKey) return 0
                    const index = sourceSnapshot.indexOfKey(key as ConversationViewportKey)
                    return index < 0 ? -1 : index + startOffset
                },
            }
        }
        return {
            length: messageRenderKeys.length + startOffset,
            keyAt(index) {
                if (startKey !== null && index === 0) return startKey
                return messageRenderKeys[index - startOffset]
            },
            indexOf(key) {
                if (startKey !== null && key === startKey) return 0
                keyLookupScans += 1
                const index = messageRenderKeys.indexOf(key)
                return index < 0 ? -1 : index + startOffset
            },
        }
    }

    function currentSourceSnapshot(): ConversationViewportSnapshot | null {
        void sourceUpdateRevision
        return activeViewportSource?.snapshot() ?? null
    }

    function currentMessageCount(snapshot = currentSourceSnapshot()): number {
        return snapshot ? snapshot.totalMessages : messages?.length ?? 0
    }

    function currentMessageKey(
        absoluteIndex: number,
        snapshot = currentSourceSnapshot(),
    ): string | undefined {
        return snapshot
            ? snapshot.keyAt(absoluteIndex)
            : messageRenderKeys[absoluteIndex]
    }

    function resetViewport(scope: string): void {
        if (activeScope !== null) navigationGeneration += 1
        clearScheduledWork()
        clearMountedRows()
        measuredHeights = new Map()
        measuredHeightIndices = new Map()
        measuredHeightIndexByKey = new Map()
        measuredHeightRecency = new Map()
        measuredHeightClock = 0
        keyLookupScans = 0
        pinReasons = new Map()
        playingMedia = new Map()
        viewportAnchor = null
        viewportResult = null
        identitySequence = null
        messageRenderKeys = []
        registeredScope = null
        registeredMessages = undefined
        registeredLength = 0
        previousReloadPointer = null
        activeScope = scope
        releaseSourcePins()
    }

    function syncIdentityRegistration(scope: string, reloadPointer: unknown): void {
        if (activeViewportSource) return
        const compatibilityMessages = messages ?? []
        const needsStructuralRegistration = (
            registeredScope !== scope
            || registeredMessages !== messages
            || compatibilityMessages.length < registeredLength
            || reloadPointer !== previousReloadPointer
        )
        if (needsStructuralRegistration) {
            identitySequence = identityRegistry.register(scope, compatibilityMessages)
        } else if (compatibilityMessages.length > registeredLength) {
            identitySequence = identityRegistry.registerAppend(
                scope,
                compatibilityMessages,
                registeredLength,
            )
        } else if (!identitySequence) {
            identitySequence = identityRegistry.register(scope, compatibilityMessages)
        }
        if (
            needsStructuralRegistration ||
            compatibilityMessages.length !== registeredLength ||
            messageRenderKeys.length === 0
        ) {
            messageRenderKeys = identitySequence.toArray()
        }
        if (needsStructuralRegistration) {
            const source = viewportKeySource(scope)
            rebuildMeasuredHeightIndices(source)
        }
        registeredScope = scope
        registeredMessages = messages
        registeredLength = compatibilityMessages.length
        previousReloadPointer = reloadPointer
    }

    function currentPins(
        currentChat: character['chats'][number] | groupChat['chats'][number] | undefined,
        sourceSnapshot: ConversationViewportSnapshot | null,
    ): ChatViewportPin[] {
        const pins: ChatViewportPin[] = []
        for (const [key, reasons] of pinReasons) {
            const indexHintText = mountedElements.get(key)?.dataset.chatViewportIndex
            const indexHint = indexHintText === undefined ? undefined : Number(indexHintText)
            for (const reason of reasons) pins.push({ key, reason, indexHint })
        }
        const totalMessages = currentMessageCount(sourceSnapshot)
        const streamingKey = totalMessages > 0
            ? currentMessageKey(totalMessages - 1, sourceSnapshot)
            : undefined
        if (currentChat?.isStreaming && streamingKey !== undefined) {
            pins.push({
                key: streamingKey,
                reason: 'streaming',
                indexHint: totalMessages - 1 + (hasConversationStart() ? 1 : 0),
            })
        }
        return pins
    }

    function contiguousRanges(indices: readonly number[]): Array<[number, number]> {
        const ranges: Array<[number, number]> = []
        for (const index of [...new Set(indices)].sort((left, right) => left - right)) {
            const current = ranges.at(-1)
            if (current && current[1] === index) current[1] = index + 1
            else ranges.push([index, index + 1])
        }
        return ranges
    }

    function syncSourcePins(
        result: ChatViewportResult,
        currentChat: character['chats'][number] | groupChat['chats'][number] | undefined,
        sourceSnapshot: ConversationViewportSnapshot | null,
    ): void {
        const source = activeViewportSource
        if (!source || !sourceSnapshot) {
            releaseSourcePins()
            return
        }
        const startOffset = hasConversationStart() ? 1 : 0
        const desired = new Map<string, {
            startIndex: number
            endIndex: number
            reason: 'viewport' | 'editor' | 'playing-media' | 'streaming'
        }>()
        const viewportIndices = result.rows.flatMap((row) => (
            row.kind === 'message' && row.index >= startOffset
                ? [row.index - startOffset]
                : []
        ))
        for (const [startIndex, endIndex] of contiguousRanges(viewportIndices)) {
            desired.set(`viewport:${startIndex}:${endIndex}`, {
                startIndex,
                endIndex,
                reason: 'viewport',
            })
        }
        for (const [key, reasons] of pinReasons) {
            const absoluteIndex = sourceSnapshot.indexOfKey(key as ConversationViewportKey)
            if (absoluteIndex < 0) continue
            for (const reason of reasons) {
                if (reason !== 'editor' && reason !== 'playing-media') continue
                desired.set(`${reason}:${absoluteIndex}:${absoluteIndex + 1}`, {
                    startIndex: absoluteIndex,
                    endIndex: absoluteIndex + 1,
                    reason,
                })
            }
        }
        if (currentChat?.isStreaming && sourceSnapshot.totalMessages > 0) {
            const absoluteIndex = sourceSnapshot.totalMessages - 1
            desired.set(`streaming:${absoluteIndex}:${absoluteIndex + 1}`, {
                startIndex: absoluteIndex,
                endIndex: absoluteIndex + 1,
                reason: 'streaming',
            })
        }

        const acquired = new Map(sourcePins)
        try {
            for (const [key, pin] of desired) {
                if (acquired.has(key)) continue
                acquired.set(key, source.acquireRangePin(
                    pin.startIndex,
                    pin.endIndex,
                    pin.reason,
                ))
            }
        } catch {
            for (const [key, pin] of acquired) {
                if (!sourcePins.has(key)) pin.release()
            }
            return
        }
        for (const [key, pin] of sourcePins) {
            if (!desired.has(key)) pin.release()
        }
        sourcePins = new Map([...acquired].filter(([key]) => desired.has(key)))
    }

    function releaseSourcePins(): void {
        for (const pin of sourcePins.values()) pin.release()
        sourcePins.clear()
    }

    function requestMissingSourceRows(
        result: ChatViewportResult,
        sourceSnapshot: ConversationViewportSnapshot | null,
    ): void {
        const source = activeViewportSource
        if (!source || !sourceSnapshot) return
        const startOffset = hasConversationStart() ? 1 : 0
        const missing = result.rows.flatMap((row) => {
            if (row.kind !== 'message' || row.index < startOffset) return []
            const absoluteIndex = row.index - startOffset
            return sourceSnapshot.rowAt(absoluteIndex) ? [] : [absoluteIndex]
        })
        for (const [startIndex, endIndex] of contiguousRanges(missing)) {
            const loadKey = [
                sourceSnapshot.sourceToken,
                sourceSnapshot.version,
                startIndex,
                endIndex,
            ].join(':')
            if (sourceLoads.has(loadKey)) continue
            const controller = new AbortController()
            sourceLoads.set(loadKey, controller)
            void source.ensureRange({
                startIndex,
                limit: endIndex - startIndex,
                reason: 'viewport',
                signal: controller.signal,
            }).catch(() => undefined).finally(() => {
                if (sourceLoads.get(loadKey) === controller) sourceLoads.delete(loadKey)
            })
        }
    }

    function abortSourceLoads(): void {
        for (const controller of sourceLoads.values()) controller.abort()
        sourceLoads.clear()
    }

    function activateViewportSource(source: ConversationViewportSource | null): void {
        if (source === activeViewportSource) return
        const nextConversationIdentity = currentConversationHandoffIdentity()
        const previousSnapshot = currentSourceSnapshot()
        const previousAnchor = captureDomAnchor()
        const preserveHandoff = (
            source !== null &&
            activeViewportSource !== null &&
            renderedConversationIdentity === nextConversationIdentity &&
            previousSnapshot !== null &&
            previousAnchor !== null
        )
        const handoffWasAtBottom = preserveHandoff
            ? pendingSourceWasAtBottom ?? checkIfAtBottom()
            : null
        let handoffAnchor: ChatViewportAnchor | null = null
        if (preserveHandoff) {
            const startOffset = hasConversationStart() ? 1 : 0
            const sourceIndex = previousSnapshot.indexOfKey(
                previousAnchor.key as ConversationViewportKey,
            )
            const absoluteIndex = sourceIndex >= 0
                ? sourceIndex
                : previousAnchor.indexHint - startOffset
            const nextSnapshot = source.snapshot()
            const remappedIndex = Math.min(
                Math.max(absoluteIndex, 0),
                nextSnapshot.totalMessages - 1,
            )
            const remappedKey = nextSnapshot.keyAt(remappedIndex)
            if (remappedKey !== undefined) {
                handoffAnchor = {
                    key: remappedKey,
                    indexHint: remappedIndex + startOffset,
                    relativeOffset: previousAnchor.relativeOffset,
                }
            }
        }
        navigationGeneration += 1
        abortSourceLoads()
        releaseSourcePins()
        sourceUnsubscribe?.()
        sourceUnsubscribe = null
        activeViewportSource = source
        sourceUpdateRevision += 1
        lastMountedSourceTailKey = null
        pendingSourceWasAtBottom = handoffWasAtBottom
        pendingSourceHandoffAnchor = handoffAnchor
        activeScope = null
        if (source) {
            sourceUnsubscribe = source.subscribe(() => {
                queueMicrotask(() => {
                    queueMicrotask(() => {
                        if (source !== activeViewportSource || !chatBody) return
                        if (lastMountedSourceTailKey !== null) {
                            pendingSourceWasAtBottom = (
                                pendingSourceWasAtBottom === true ||
                                isMountedKeyAtBottom(lastMountedSourceTailKey)
                            )
                        }
                        sourceUpdateRevision += 1
                        reconcileViewport({
                            anchor: pendingSourceHandoffAnchor ?? undefined,
                        })
                        if (
                            pendingSourceHandoffAnchor &&
                            mountedElements.has(pendingSourceHandoffAnchor.key)
                        ) {
                            const completedAnchor = pendingSourceHandoffAnchor
                            scheduleFrame(() => {
                                if (pendingSourceHandoffAnchor === completedAnchor) {
                                    pendingSourceHandoffAnchor = null
                                }
                            })
                        }
                    })
                })
            })
        }
        if (chatBody) reconcileViewport({ anchor: handoffAnchor ?? undefined })
    }

    $effect.pre(() => {
        activateViewportSource(viewportSource)
    })

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
        renderedConversationIdentity = currentConversationHandoffIdentity()
        if (activeScope !== scope) resetViewport(scope)
        const reloadPointerMap = get(ReloadChatPointer)
        const sourceSnapshot = currentSourceSnapshot()
        syncIdentityRegistration(scope, reloadPointerMap)
        const keySource = viewportKeySource(scope, sourceSnapshot)
        const preservedAnchor = options.anchor !== undefined
            ? options.anchor
            : options.preserveAnchor === false
                ? viewportAnchor
                : pendingSourceHandoffAnchor ?? captureDomAnchor()
        const currentChat = currentCharacter.chats?.[currentCharacter.chatPage]
        const budget = getRuntimePerformanceBudgets().chatMountedMessageBudget
        const result = buildChatViewport({
            keySource,
            budget,
            overscan: Math.min(VIEWPORT_OVERSCAN, budget - 1),
            estimatedMessageHeight: ESTIMATED_MESSAGE_HEIGHT,
            measuredHeightsByIndex: measuredHeightIndices,
            anchor: preservedAnchor,
            jumpTarget: options.jumpTarget,
            pins: currentPins(currentChat, sourceSnapshot),
        })
        viewportAnchor = result.anchor
        viewportResult = result
        syncSourcePins(result, currentChat, sourceSnapshot)
        requestMissingSourceRows(result, sourceSnapshot)
        renderViewportRows(scope, result, currentChat, reloadPointerMap, sourceSnapshot)
        chatBody.dataset.chatPinOverflow = String(result.pinOverflow?.count ?? 0)
        chatBody.dataset.chatMeasuredHeightCount = String(measuredHeights.size)
        chatBody.dataset.chatKeyLookupScans = String(keyLookupScans)
        correctDomAnchor(preservedAnchor)
        hasRenderedChat = true
        return result
    }

    function renderViewportRows(
        scope: string,
        result: ChatViewportResult,
        currentChat: character['chats'][number] | groupChat['chats'][number] | undefined,
        reloadPointerMap: Record<number, number>,
        sourceSnapshot: ConversationViewportSnapshot | null,
    ): void {
        const currentRenderKeys = new Set<string>()
        const orderedElements: HTMLElement[] = []
        const configuredPerformanceMode = DBState.db.streamingDisplayOptimizationMode ?? 'off'
        const performanceMode = currentChat?.isStreaming
            ? currentChat.activeStreamingDisplayOptimizationMode ?? configuredPerformanceMode
            : configuredPerformanceMode
        const totalMessages = currentMessageCount(sourceSnapshot)
        const activeStreamingIndex = performanceMode !== 'off' && currentChat?.isStreaming
            ? totalMessages - 1
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
                renderConversationStart(key, element, totalMessages)
                element.classList.remove('is-latest-chat-row', 'is-settled-history')
                orderedElements.push(element)
                continue
            }

            const index = row.index - startOffset
            const viewportRow = sourceSnapshot?.rowAt(index)
            const message = (
                sourceSnapshot ? viewportRow?.message : messages?.[index]
            ) as Message | undefined
            const key = row.key
            if (!message) {
                const retainedElement = mountedElements.get(key)
                if (retainedElement) {
                    currentRenderKeys.add(key)
                    orderedElements.push(retainedElement)
                }
                continue
            }
            const element = ensureElement(key, row.index)
            element.dataset.chatIndex = String(index)
            currentRenderKeys.add(key)
            const messageLargePortrait = message.role === 'user'
                ? (userIconPortrait ?? false)
                : ((currentCharacter as character).largePortrait ?? false)
            const activeStreamingMessage = index === activeStreamingIndex && message.role === 'char'
            const bookmarked = currentChat?.bookmarks?.includes(message.chatId ?? '') ?? false
            const renderSignature = createChatRenderSignature({
                message,
                index,
                totalLength: totalMessages,
                largePortrait: messageLargePortrait,
                reloadPointer: reloadPointerMap[index] ?? 0,
                globalReloadPointer,
                activeStreamingMessage,
                bookmarked,
                resolvedImage: message.role === 'user' ? resolvedUserImage : resolvedCharacterImage,
                displayName: message.role === 'user' ? currentUsername : currentCharacter.name,
                parserCharacter,
                parserCharacterStamp,
            })
            const previousSignature = renderSignatures.get(key)
            let parserProjection: BoundedLiveChatParserProjection | undefined
            if (sourceSnapshot && viewportRow && activeViewportSource && parserProjectionResolver) {
                const projectionState = ensureRowParserProjection(
                    key,
                    viewportRow,
                    totalMessages,
                    renderSignature,
                    sourceSnapshot,
                )
                if (!projectionState.projection) {
                    orderedElements.push(element)
                    continue
                }
                if (projectionState.projection.kind === 'bounded') {
                    parserProjection = projectionState.projection
                }
            }
            if (!areChatRenderSignaturesEqual(previousSignature, renderSignature)) {
                const source = activeViewportSource
                releaseRowRuntimeState(key, true)
                unmountInstance(key)
                element.replaceChildren()
                const instance = mount(Chat, {
                    target: element,
                    props: {
                        message: message.data,
                        viewportRow,
                        captureViewportTarget: viewportRow && source
                            ? () => source.captureMessageTarget(viewportRow.key)
                            : undefined,
                        bookmarked,
                        isLastMemory: false,
                        idx: index,
                        totalLength: totalMessages,
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
                        parserProjection,
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
            const latest = index === totalMessages - 1
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
        if (sourceSnapshot && totalMessages > 0) {
            const tailIndex = totalMessages - 1
            const tailKey = sourceSnapshot.keyAt(tailIndex)
            if (
                tailKey !== undefined &&
                sourceSnapshot.rowAt(tailIndex) !== undefined &&
                mountedElements.has(tailKey)
            ) lastMountedSourceTailKey = tailKey
        } else if (!sourceSnapshot) {
            lastMountedSourceTailKey = null
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

    function renderConversationStart(
        key: string,
        element: HTMLElement,
        totalMessages: number,
    ): void {
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
            totalMessages,
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
                totalMessages,
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

    function releaseRowRuntimeState(key: string, preserveParserProjection = false): void {
        if (!preserveParserProjection) releaseRowParserProjection(key)
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

    function ensureRowParserProjection(
        key: string,
        row: ConversationViewportRow,
        totalMessages: number,
        renderSignature: ChatRenderSignature,
        sourceSnapshot: ConversationViewportSnapshot,
    ): RowParserProjectionState {
        const source = activeViewportSource!
        const existing = rowParserProjections.get(key)
        if (
            existing
            && existing.source === source
            && existing.sourceToken === sourceSnapshot.sourceToken
            && existing.sourceVersion === sourceSnapshot.version
            && existing.row.key === row.key
            && existing.row.absoluteIndex === row.absoluteIndex
            && existing.row.sourceVersion === row.sourceVersion
            && existing.totalMessages === totalMessages
            && areChatRenderSignaturesEqual(existing.renderSignature, renderSignature)
        ) return existing

        releaseRowRuntimeState(key)
        unmountInstance(key)
        renderSignatures.delete(key)
        const controller = new AbortController()
        const state: RowParserProjectionState = {
            controller,
            navigationGeneration,
            source,
            sourceToken: sourceSnapshot.sourceToken,
            sourceVersion: sourceSnapshot.version,
            row,
            totalMessages,
            renderSignature,
            projection: null,
            failed: false,
            retryTimer: null,
        }
        rowParserProjections.set(key, state)
        void parserProjectionResolver!.resolve({
            row,
            totalMessages,
            signal: controller.signal,
            isCurrent: () => isRowParserProjectionCurrent(key, state),
        }).then((projection) => {
            if (!isRowParserProjectionCurrent(key, state)) {
                if (projection.kind === 'complete') projection.release()
                return
            }
            state.projection = projection
            reconcileViewport()
        }).catch(() => {
            if (!isRowParserProjectionCurrent(key, state)) return
            state.failed = true
            state.retryTimer = setTimeout(() => {
                if (!isRowParserProjectionCurrent(key, state)) return
                rowParserProjections.delete(key)
                state.controller.abort()
                reconcileViewport()
            }, PARSER_PROJECTION_RETRY_DELAY_MS)
        })
        return state
    }

    function isRowParserProjectionCurrent(
        key: string,
        state: RowParserProjectionState,
    ): boolean {
        if (
            state.controller.signal.aborted
            || rowParserProjections.get(key) !== state
            || navigationGeneration !== state.navigationGeneration
            || activeViewportSource !== state.source
        ) return false
        const snapshot = currentSourceSnapshot()
        if (
            !snapshot
            || snapshot.sourceToken !== state.sourceToken
            || snapshot.version !== state.sourceVersion
            || snapshot.totalMessages !== state.totalMessages
        ) return false
        const currentRow = snapshot.rowAt(state.row.absoluteIndex)
        return currentRow?.key === state.row.key
            && currentRow.sourceVersion === state.row.sourceVersion
    }

    function releaseRowParserProjection(key: string): void {
        const state = rowParserProjections.get(key)
        if (!state) return
        rowParserProjections.delete(key)
        state.controller.abort()
        if (state.retryTimer !== null) clearTimeout(state.retryTimer)
        if (state.projection?.kind === 'complete') state.projection.release()
        state.projection = null
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

    function rebuildMeasuredHeightIndices(source = viewportKeySource(currentChatScope())): void {
        const remainingKeys = new Set(measuredHeights.keys())
        const nextByIndex = new Map<number, number>()
        const nextIndexByKey = new Map<string, number>()
        for (let index = 0; index < source.length && remainingKeys.size > 0; index++) {
            const key = source.keyAt(index)
            if (key === undefined || !remainingKeys.delete(key)) continue
            nextByIndex.set(index, measuredHeights.get(key)!)
            nextIndexByKey.set(key, index)
        }
        for (const key of remainingKeys) {
            measuredHeights.delete(key)
            measuredHeightRecency.delete(key)
        }
        measuredHeightIndices = nextByIndex
        measuredHeightIndexByKey = nextIndexByKey
    }

    function pruneMeasuredHeights(): void {
        if (measuredHeights.size <= MEASURED_HEIGHT_CACHE_LIMIT) {
            chatBody.dataset.chatMeasuredHeightCount = String(measuredHeights.size)
            return
        }
        const retained = new Set([
            ...renderKeys,
            ...pinReasons.keys(),
            ...(viewportAnchor ? [viewportAnchor.key] : []),
        ])
        const candidates = [...measuredHeights.keys()]
            .filter((key) => !retained.has(key))
            .sort((left, right) => (
                (measuredHeightRecency.get(left) ?? 0) -
                (measuredHeightRecency.get(right) ?? 0)
            ))
        for (const key of candidates) {
            if (measuredHeights.size <= MEASURED_HEIGHT_CACHE_LIMIT) break
            measuredHeights.delete(key)
            measuredHeightRecency.delete(key)
            const index = measuredHeightIndexByKey.get(key)
            if (index !== undefined) measuredHeightIndices.delete(index)
            measuredHeightIndexByKey.delete(key)
        }
        chatBody.dataset.chatMeasuredHeightCount = String(measuredHeights.size)
    }

    function handleResize(entries: ResizeObserverEntry[]): void {
        const anchor = captureDomAnchor()
        let changed = false
        for (const entry of entries) {
            const element = entry.target as HTMLElement
            const key = element.dataset.chatRenderKey
            const height = element.getBoundingClientRect().height || entry.contentRect.height
            if (!key || !Number.isFinite(height) || height <= 0) continue
            const index = Number(element.dataset.chatViewportIndex)
            if (!Number.isInteger(index) || index < 0) continue
            if (Math.abs((measuredHeights.get(key) ?? 0) - height) < 0.5) continue
            const previousIndex = measuredHeightIndexByKey.get(key)
            if (previousIndex !== undefined && previousIndex !== index) {
                measuredHeightIndices.delete(previousIndex)
            }
            measuredHeights.set(key, height)
            measuredHeightRecency.set(key, ++measuredHeightClock)
            measuredHeightIndices.set(index, height)
            measuredHeightIndexByKey.set(key, index)
            changed = true
        }
        if (!changed) return
        pruneMeasuredHeights()
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
        const keySource = viewportKeySource(currentChatScope())
        if (!Number.isInteger(target) || target < 0 || target >= keySource.length) return
        const key = keySource.keyAt(target)
        if (key === undefined) return
        viewportAnchor = {
            key,
            indexHint: target,
            relativeOffset: viewportAnchor?.relativeOffset ?? 0,
        }
        reconcileViewport({ jumpTarget: target })
    }

    function isMountedKeyAtBottom(key: string): boolean {
        if (!scrollContainer) return true
        const element = mountedElements.get(key)
        if (!element) return false
        return element.getBoundingClientRect().top <= scrollContainer.getBoundingClientRect().bottom + 100
    }

    function checkIfAtBottom(): boolean {
        const sourceSnapshot = currentSourceSnapshot()
        const totalMessages = currentMessageCount(sourceSnapshot)
        if (!scrollContainer || totalMessages === 0) return true
        const latestKey = currentMessageKey(totalMessages - 1, sourceSnapshot)
        if (latestKey === undefined) return false
        return isMountedKeyAtBottom(latestKey)
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
        const source = activeViewportSource
        const sourceSnapshot = currentSourceSnapshot()
        const totalMessages = currentMessageCount(sourceSnapshot)
        if (!Number.isInteger(index) || index < 0 || index >= totalMessages) return false
        const generation = ++navigationGeneration
        const scope = currentChatScope()
        if (source && sourceSnapshot) {
            const budget = getRuntimePerformanceBudgets().chatMountedMessageBudget
            const startIndex = Math.max(0, index - Math.min(VIEWPORT_OVERSCAN, budget - 1))
            const controller = new AbortController()
            const loadKey = `jump:${generation}`
            sourceLoads.set(loadKey, controller)
            try {
                await source.ensureRange({
                    startIndex,
                    limit: Math.min(budget, totalMessages - startIndex),
                    reason: 'jump',
                    signal: controller.signal,
                })
            } catch {
                return false
            } finally {
                if (sourceLoads.get(loadKey) === controller) sourceLoads.delete(loadKey)
            }
            const currentSnapshot = currentSourceSnapshot()
            if (
                controller.signal.aborted ||
                generation !== navigationGeneration ||
                source !== activeViewportSource ||
                currentSnapshot?.sourceToken !== sourceSnapshot.sourceToken ||
                currentSnapshot.version !== sourceSnapshot.version
            ) return false
        }
        const startOffset = hasConversationStart() ? 1 : 0
        const result = reconcileViewport({ jumpTarget: index + startOffset, preserveAnchor: false })
        if (!result?.jumpAccepted) return false
        const key = currentMessageKey(index)
        if (key === undefined) return false
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
        const totalMessages = currentMessageCount()
        if (totalMessages > 0) {
            await jumpTo(totalMessages - 1)
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
    let previousConversationIdentity: string | null = null

    $effect(() => {
        void $ReloadChatPointer
        if (!imagesReady && !hasRenderedChat) return
        const wasAtBottom = pendingSourceWasAtBottom ?? checkIfAtBottom()
        pendingSourceWasAtBottom = null
        reconcileViewport()
        const conversationIdentity = currentConversationHandoffIdentity()
        const isSameChat = conversationIdentity === previousConversationIdentity
        const totalMessages = currentMessageCount()
        if (isSameChat && totalMessages > previousLength) {
            const snapshot = currentSourceSnapshot()
            const lastMessage = snapshot
                ? snapshot.rowAt(totalMessages - 1)?.message
                : messages?.at(-1)
            if (snapshot && !lastMessage) {
                pendingSourceWasAtBottom = wasAtBottom
                previousConversationIdentity = conversationIdentity
                return
            }
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
        previousLength = totalMessages
        previousConversationIdentity = conversationIdentity
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
        abortSourceLoads()
        releaseSourcePins()
        sourceUnsubscribe?.()
        sourceUnsubscribe = null
        activeViewportSource = null
        clearScheduledWork()
        clearMountedRows()
        renderSignatures.clear()
        measuredHeights.clear()
        measuredHeightIndices.clear()
        measuredHeightIndexByKey.clear()
        measuredHeightRecency.clear()
        pinReasons.clear()
        playingMedia.clear()
        imageResolutionGeneration += 1
    })

</script>

<div class="flex flex-col-reverse" bind:this={chatBody}></div>
