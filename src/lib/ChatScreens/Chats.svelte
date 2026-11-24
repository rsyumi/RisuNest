<script lang="ts">
    import { CbsHistoryDependencies, hasCbsHistoryChanges } from 'src/ts/cbsHistoryAccess'
    import type { character, Message, StreamingDisplayOptimizationMode } from 'src/ts/storage/database.svelte'
    // settled() resolves once the DOM update commits; tick() would also wait for a frame or timer.
    import { mount, onDestroy, onMount, settled as domSettled, unmount, untrack } from 'svelte'
    import { get } from 'svelte/store'
    import Chat from './Chat.svelte'
    import { chatScreenState } from 'src/ts/ui/chatScreenState.svelte'
    import type { ChatDisplayRefresh, ChatPresentationRefresh } from 'src/ts/chatDisplayRefresh'
    import ChatConversationStart from './ChatConversationStart.svelte'
    import { chatWindowLayerContext } from './ChatWindowLayer.svelte'
    import { getCharImage } from 'src/ts/characters'
    import { createSimpleCharacter, DBState, selectedCharID, ReloadChatPointer, ReloadGUIPointer } from 'src/ts/stores.svelte'
    import {
        areChatRenderSignaturesEqual,
        canRefreshChatRenderInPlace,
        ChatRenderIdentityRegistry,
        type ChatRenderIdentitySequence,
        createChatParserDependencyStamp,
        createChatRenderSignature,
        type ChatRenderSignature,
    } from 'src/ts/chatRenderIdentity'
    import {
        buildChatViewport,
        measureChatViewportRange,
        locateChatViewportOffset,
        resolveChatViewportStep,
        type ChatViewportAnchor,
        type ChatViewportJumpOptions,
        type ChatViewportKeySource,
        type ChatViewportPin,
        type ChatViewportPinReason,
        type ChatViewportResult,
        type ChatViewportRowBounds,
        type ChatViewportStepDirection,
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
        LiveChatParserConversationStartRequest,
    } from 'src/ts/selectedConversationLiveParserProjection'
    import type { SelectedConversationOperations } from 'src/ts/selectedConversationOperations'
    import { yieldToMainThread } from 'src/ts/ui/yieldToUi'
    import { nextDisplayListenerOrder } from 'src/ts/process/luaDisplayTimeouts'
    import LoadingIndicator from 'src/lib/UI/GUI/LoadingIndicator.svelte'
    import { language } from 'src/lang'
    import {
        discardEditorDraft,
        discardEditorDraftsExcept,
        keepEditorDraft,
        pendingEditorDrafts,
        takeEditorDraft,
        type ChatEditorDraft,
    } from 'src/ts/chatEditorDrafts'
    import cloneDeep from 'lodash/cloneDeep'
    import isEqual from 'lodash/isEqual'
    import { chatViewEvents } from 'src/ts/plugins/chatViewHost.svelte'
    import { reconcileChatViewportChildren } from 'src/ts/chatViewportDom'

    let {
        messages,
        currentCharacter,
        onReroll,
        onNextReroll = onReroll,
        unReroll,
        onFirstMessageReroll = () => {},
        unFirstMessageReroll = () => {},
        onRemoveCreatorQuote = () => {},
        showAiWarning = false,
        currentUsername,
        userIcon,
        userIconPortrait,
        viewportSource = null,
        viewportNavigationGeneration = 0,
        parserProjectionResolver,
        acquireConversationStartParserLease,
        selectedConversationOperations,
        hasNewUnreadMessage = $bindable(false),
        onScrollMove,
    }: {
        messages?: Message[]
        currentCharacter: character
        onReroll: () => void
        onNextReroll?: () => void
        unReroll: () => void
        onFirstMessageReroll?: () => void
        unFirstMessageReroll?: () => void
        onRemoveCreatorQuote?: () => void
        showAiWarning?: boolean
        currentUsername: string
        userIcon: string
        userIconPortrait?: boolean
        viewportSource?: ConversationViewportSource | null
        viewportNavigationGeneration?: number
        parserProjectionResolver?: LiveChatParserProjectionResolver
        acquireConversationStartParserLease?: (
            request: LiveChatParserConversationStartRequest,
        ) => Promise<{ release(): void } | null>
        selectedConversationOperations?: SelectedConversationOperations
        hasNewUnreadMessage?: boolean
        onScrollMove?: () => void
    } = $props()
    const windowLayerContext = chatWindowLayerContext()

    const ESTIMATED_MESSAGE_HEIGHT = 256
    const VIEWPORT_OVERSCAN = 8
    const MEASURED_HEIGHT_CACHE_LIMIT = 256
    const PARSER_PROJECTION_RETRY_DELAY_MS = 250
    const PARSER_PROJECTION_MAX_ATTEMPTS = 3
    const CHAT_MOUNT_BATCH_SIZE = 4

    type ChatInstance = {
        updateCandidatePosition?: (page: number, total: number) => void
        hasStreamingPreview?: () => boolean
        updatePresentation?: (state: ChatPresentationRefresh) => void
        refreshMessageDisplay?: (state: ChatDisplayRefresh) => void
        hasActiveEditor?: () => boolean
        captureEditorDraft?: () => Omit<ChatEditorDraft, 'caret'> | null
        restoreEditor?: (draft: ChatEditorDraft) => void
        refreshParserProjection?: (projection?: BoundedLiveChatParserProjection) => void
        refreshConversationStartParser?: (totalMessages?: number, snapshot?: ConversationViewportSnapshot, forced?: boolean) => void
        updateConversationStartPresentation?: (state: { resolvedImage: string }) => void
        updateStreamingDisplay?: (state: {
            isOptimizedStreamingMessage: boolean
            streamingOptimizationMode: StreamingDisplayOptimizationMode
            rawStreamingText: string
        }) => void
        updateViewportBinding?: (state: {
            viewportRow: ConversationViewportRow
            viewportSourceToken: string
            captureViewportTarget: () => ReturnType<ConversationViewportSource['captureMessageTarget']>
            parserProjection?: BoundedLiveChatParserProjection
            totalMessages: number
            preserveDisplay?: boolean
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
    let containerWidth = 0
    let containerHeight = 0
    let keyLookupScans = 0
    let pinReasons = new Map<string, Set<ChatViewportPinReason>>()
    let blurredEditorPins = new Set<string>()
    let playingMedia = new Map<string, Set<EventTarget>>()
    let sourcePins = new Map<string, ConversationSourcePin>()
    let sourceLoads = new Map<string, AbortController>()
    let latestPairIndices: number[] = []
    let latestPairLookup: Promise<number[]> | null = null
    let latestPairController = new AbortController()
    interface RowParserProjectionState {
        dependencies: CbsHistoryDependencies
        readonly controller: AbortController
        readonly source: ConversationViewportSource
        readonly sourceToken: string
        readonly sourceVersion: number
        historyVersion?: number
        readonly row: ConversationViewportRow
        readonly totalMessages: number
        readonly renderSignature: ChatRenderSignature
        projection: LiveChatParserProjection | null
        preparationPending: boolean
        displayStarted: boolean
        needsRemount: boolean
        preserveMountedRuntime: boolean
        failed: boolean
        failureCount: number
        retryTimer: ReturnType<typeof setTimeout> | null
        /** When the row asked for its display, taken by the mount that follows its preparation. */
        displayRequestOrder?: number
    }
    let rowParserProjections = new Map<string, RowParserProjectionState>()
    interface PendingRowMount {
        readonly key: string
        readonly element: HTMLElement
        readonly priority: number
        readonly order: number
        readonly isCurrent: () => boolean
        readonly run: (settled: () => void) => void
        readonly parserProjectionState?: RowParserProjectionState
        /** When the view asked for this row, kept while the row waits to mount. */
        readonly displayRequestOrder: number
    }
    interface RowMountWaiter {
        readonly navigationGeneration: number
        readonly resolve: (mounted: boolean) => void
    }
    let pendingRowMounts = new Map<string, PendingRowMount>()
    const activeRowPreparations = new Map<string, object>()
    let rowMountWaiters = new Map<string, Set<RowMountWaiter>>()
    let mountQueueOrder = 0
    let mountQueueGeneration = 0
    let mountDrainActive = false
    let mountDrainQueued = false
    let projectionReconcileQueued = false
    let projectionReconcileGeneration = 0
    let destroyed = false
    const chatView = chatViewEvents.createReporter()
    let hasMountedUsableRow = false
    let initialRowsLoading = $state(false)
    let initialRowsLoadFailed = $state(false)
    let parserProjectionLoadFailed = $state(false)
    let historyRowsLoadFailed = $state(false)
    let latestPairLoadFailed = $state(false)
    let initialLatestFollow = false
    let initialLatestMessageCount: number | null = null
    const USER_SCROLL_INTENT_MS = 1000
    let userScrollIntentAt = Number.NEGATIVE_INFINITY
    let scrollPointerHeld = false
    let sourceHandoffRuntimeKeys = new Set<string>()
    let sourceUnsubscribe: (() => void) | null = null
    let activeViewportSource: ConversationViewportSource | null = null
    let activeViewportNavigationGeneration = 0
    let sourceUpdateRevision = $state(0)
    let lastMountedSourceTailKey: string | null = null
    let pendingSourceWasAtBottom: boolean | null = null
    let pendingSourceHandoffAnchor: ChatViewportAnchor | null = null
    let pendingMissingRowsAnchor: ChatViewportAnchor | null = null
    let renderedConversationIdentity: string | null = null
    const renderedMessageIds = new Map<string, string | undefined>()
    let renderedStartOffset = 0
    let renderedOwnerId: string | null = null
    let renderedConversationId: string | null = null
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
    let positionGeneration = 0
    let pendingJump: {
        generation: number
        conversationIdentity: string
        index: number
        sourceSnapshot: ConversationViewportSnapshot | null
        message: Readonly<Message> | null
    } | null = null
    let suppressScroll = false
    let finishViewportLayoutChange: (() => void) | null = null
    const identityRegistry = new ChatRenderIdentityRegistry()
    const ownerSessionIds = new WeakMap<object, number>()
    const conversationSessionIds = new WeakMap<object, number>()
    let nextScopeIdentity = 1

    let resolvedCharacterImage = $state<string | null>(null)
    let resolvedUserImage = $state<string | null>(null)
    let imagesReady = $state(false)
    let imageResolutionGeneration = 0
    let imageOwner: string | undefined
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
        if (imageOwner !== currentCharacter.chaId) {
            imageOwner = currentCharacter.chaId
            resolvedCharacterImage = null
            resolvedUserImage = null
        }
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
        const ownerId = `character:${selectedCharacterIndex}:${currentCharacter.chaId}`
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
        const ownerId = `character:${currentCharacter.chaId}`
        const conversationId = currentChat?.id ?? `page:${currentCharacter.chatPage}`
        return [ownerId, conversationId]
            .map((part) => `${part.length}:${part}`)
            .join('|')
    }

    function currentChatScope(): string {
        if (activeViewportSource) {
            return `viewport:${currentConversationHandoffIdentity()}`
        }
        return `${currentConversationIdentity()}|19:compatibility-array`
    }

    function objectScopeIdentity(identities: WeakMap<object, number>, value: object): number {
        const existing = identities.get(value)
        if (existing !== undefined) return existing
        const identity = nextScopeIdentity++
        identities.set(value, identity)
        return identity
    }

    function hasConversationStart(): boolean {
        return true
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

    function resetViewport(scope: string, resetScroll: boolean): void {
        if (resetScroll) retainScreenAnchor()
        renderedMessageIds.clear()
        if (activeScope !== null) {
            navigationGeneration += 1
            cancelStaleRowMountWaiters()
        }
        clearScheduledWork()
        clearMountedRows()
        if (resetScroll) {
            initialLatestFollow = true
            initialLatestMessageCount = null
            keepInitialLatestPosition()
        }
        measuredHeights = new Map()
        measuredHeightIndices = new Map()
        measuredHeightIndexByKey = new Map()
        measuredHeightRecency = new Map()
        measuredHeightClock = 0
        keyLookupScans = 0
        pinReasons = new Map()
        blurredEditorPins = new Set()
        playingMedia = new Map()
        sourceHandoffRuntimeKeys = new Set()
        hasMountedUsableRow = false
        initialRowsLoading = false
        initialRowsLoadFailed = false
        parserProjectionLoadFailed = false
        historyRowsLoadFailed = false
        viewportAnchor = null
        pendingMissingRowsAnchor = null
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
        currentChat: character['chats'][number] | undefined,
        sourceSnapshot: ConversationViewportSnapshot | null,
    ): ChatViewportPin[] {
        const pins: ChatViewportPin[] = []
        for (const [key, reasons] of pinReasons) {
            const indexHintText = mountedElements.get(key)?.dataset.chatViewportIndex
            const indexHint = indexHintText === undefined ? undefined : Number(indexHintText)
            for (const reason of reasons) pins.push({ key, reason, indexHint })
        }
        const totalMessages = currentMessageCount(sourceSnapshot)
        const pair = sourceSnapshot ? latestPairIndices : []
        if (!sourceSnapshot) {
            const roles = new Set<string>()
            for (let index = totalMessages - 1; index >= 0 && roles.size < 2; index--) {
                const message = messages?.[index]
                if (message && !message.isComment && !message.disabled &&
                    (message.role === 'user' || message.role === 'char') && !roles.has(message.role)) {
                    roles.add(message.role)
                    pair.push(index)
                }
            }
        }
        for (const index of pair) {
            const key = currentMessageKey(index, sourceSnapshot)
            if (key !== undefined) pins.push({ key, reason: 'latest-pair', indexHint: index + (hasConversationStart() ? 1 : 0) })
        }
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

    function updateLatestPair(): void {
        const source = activeViewportSource
        if (!source?.findLatestMessagePair || latestPairLoadFailed) return
        const snapshot = source.snapshot()
        if (!hasMountedUsableRow && snapshot.totalMessages > 0 && !snapshot.rowAt(snapshot.totalMessages - 1)) return
        const lookup = source.findLatestMessagePair(latestPairController.signal)
        if (lookup === latestPairLookup) return
        latestPairLookup = lookup
        void lookup.then((indices) => {
            if (destroyed || source !== activeViewportSource || latestPairLookup !== lookup) return
            if (source.snapshot().version !== snapshot.version) {
                latestPairLookup = null
                queueProjectionReconcile()
                return
            }
            if (indices.length === latestPairIndices.length && indices.every((index, offset) => latestPairIndices[offset] === index)) return
            latestPairIndices = indices
            queueProjectionReconcile()
        }).catch((error) => {
            if (destroyed || source !== activeViewportSource || latestPairLookup !== lookup || latestPairController.signal.aborted) return
            const current = source.snapshot()
            if (error instanceof DOMException && error.name === 'AbortError' &&
                (current.version !== snapshot.version || current.storeRevision !== snapshot.storeRevision)) {
                latestPairLookup = null
                queueProjectionReconcile()
                return
            }
            latestPairLoadFailed = true
        })
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
        currentChat: character['chats'][number] | undefined,
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
        if (historyRowsLoadFailed || (initialRowsLoadFailed && !hasMountedUsableRow)) return
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
            }).catch(() => {
                const currentSnapshot = currentSourceSnapshot()
                if (
                    controller.signal.aborted
                    || source !== activeViewportSource
                    || currentSnapshot?.sourceToken !== sourceSnapshot.sourceToken
                    || currentSnapshot.version !== sourceSnapshot.version
                ) return
                if (hasMountedUsableRow) {
                    historyRowsLoadFailed = true
                    return
                }
                initialRowsLoading = false
                initialRowsLoadFailed = true
            }).finally(() => {
                if (sourceLoads.get(loadKey) === controller) sourceLoads.delete(loadKey)
            })
        }
    }

    function retryInitialSourceRows(): void {
        if (!activeViewportSource) return
        initialRowsLoadFailed = false
        historyRowsLoadFailed = false
        latestPairLoadFailed = false
        latestPairLookup = null
        initialRowsLoading = !hasMountedUsableRow && currentMessageCount() > 0
        for (const [key, state] of rowParserProjections) {
            if (state.failed) releaseRowParserProjection(key)
        }
        abortSourceLoads()
        reconcileViewport({
            anchor: pendingMissingRowsAnchor ?? viewportAnchor,
        })
    }

    function abortSourceLoads(): void {
        for (const controller of sourceLoads.values()) controller.abort()
        sourceLoads.clear()
    }

    function activateViewportSource(source: ConversationViewportSource | null): void {
        if (source === activeViewportSource) return
        const nextConversationIdentity = currentConversationHandoffIdentity()
        const previousSnapshot = currentSourceSnapshot()
        const nextSnapshot = source?.snapshot() ?? null
        const previousAnchor = captureDomAnchor()
        const preserveHandoff = (
            source !== null &&
            activeViewportSource !== null &&
            activeViewportNavigationGeneration === viewportNavigationGeneration &&
            renderedConversationIdentity === nextConversationIdentity &&
            previousSnapshot !== null &&
            previousAnchor !== null
        )
        const jump = currentPendingJump()
        const nextJumpKey = jump ? nextSnapshot?.keyAt(jump.index) : undefined
        const nextJumpTarget =
            nextJumpKey !== undefined ? source?.captureMessageTarget(nextJumpKey) : null
        // A completed history lease may replace the source while a jump mounts
        // its rows. Continue only when the active session proves the same target.
        const preserveJump =
            preserveHandoff &&
            jump !== null &&
            jump.message !== null &&
            jump.sourceSnapshot !== null &&
            previousSnapshot?.sourceToken === jump.sourceSnapshot.sourceToken &&
            previousSnapshot?.version === jump.sourceSnapshot.version &&
            nextSnapshot?.totalMessages === jump.sourceSnapshot.totalMessages &&
            nextSnapshot?.storeRevision === jump.sourceSnapshot.storeRevision &&
            nextJumpTarget?.kind === 'session' &&
            nextJumpTarget.absoluteIndex === jump.index &&
            nextJumpTarget.character === currentCharacter &&
            nextJumpTarget.conversation ===
                currentCharacter.chats[currentCharacter.chatPage] &&
            isEqual(nextJumpTarget.message, jump.message)
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
        if (preserveJump) {
            jump.generation = navigationGeneration
            jump.sourceSnapshot = nextSnapshot
        }
        cancelStaleRowMountWaiters()
        abortSourceLoads()
        releaseSourcePins()
        sourceUnsubscribe?.()
        sourceUnsubscribe = null
        if (preserveHandoff && previousSnapshot && nextSnapshot) {
            remapMountedSourceRows(nextSnapshot)
        }
        activeViewportSource = source
        latestPairController.abort()
        latestPairController = new AbortController()
        latestPairLookup = null
        latestPairIndices = []
        latestPairLoadFailed = false
        activeViewportNavigationGeneration = viewportNavigationGeneration
        sourceUpdateRevision += 1
        lastMountedSourceTailKey = null
        pendingSourceWasAtBottom = handoffWasAtBottom
        pendingSourceHandoffAnchor = handoffAnchor
        if (!preserveHandoff) {
            activeScope = null
            historyRowsLoadFailed = false
        }
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
        let pending: typeof closest = null
        for (const [key, element] of mountedElements) {
            if (!element.isConnected) continue
            const indexText = element.dataset.chatViewportIndex
            if (indexText === undefined) continue
            const index = Number(indexText)
            const rect = element.getBoundingClientRect()
            if (rect.bottom <= rect.top || rect.bottom <= containerRect.top || rect.top >= containerRect.bottom) continue
            const offset = rect.top - containerRect.top
            const distance = Math.abs(offset)
            if (element.dataset.chatHeightPending === 'true') {
                if (!pending || distance < pending.distance) pending = { key, index, offset, distance }
            } else if (!closest || distance < closest.distance) closest = { key, index, offset, distance }
        }
        closest ??= pending
        return closest
            ? { key: closest.key, indexHint: closest.index, relativeOffset: closest.offset }
            : viewportAnchor
    }

    function correctDomAnchor(anchor: ChatViewportAnchor | null): void {
        if (initialLatestFollow || currentPendingJump()) return
        if (!anchor || !scrollContainer) return
        const element = mountedElements.get(anchor.key)
        if (!element) return
        const currentOffset =
            element.getBoundingClientRect().top -
            scrollContainer.getBoundingClientRect().top
        const delta = currentOffset - anchor.relativeOffset
        if (Math.abs(delta) < 0.5 || typeof scrollContainer.scrollBy !== 'function') {
            lastScrollTop = scrollContainer.scrollTop
            return
        }
        suppressScroll = true
        const correctionGeneration = positionGeneration
        scrollContainer.scrollBy({ top: delta, behavior: 'instant' })
        lastScrollTop = scrollContainer.scrollTop
        scheduleFrame(() => {
            if (correctionGeneration !== positionGeneration) return
            suppressScroll = false
            if (scrollContainer) lastScrollTop = scrollContainer.scrollTop
        })
    }

    function beginViewportLayoutChange(): void {
        if (!scrollContainer || initialLatestFollow || currentPendingJump() || finishViewportLayoutChange) return
        const finish = () => {
            if (finishViewportLayoutChange !== finish) return
            finishViewportLayoutChange = null
        }
        finishViewportLayoutChange = finish
        scheduleFrame(() => {
            finish()
            // Process input that arrived during the structural change using the final layout.
            handleScroll()
        })
    }

    function reconcileViewport(options: {
        jumpTarget?: number
        preserveAnchor?: boolean
        anchor?: ChatViewportAnchor | null
    } = {}): ChatViewportResult | null {
        if (!chatBody) return null
        if (projectionReconcileQueued) {
            projectionReconcileQueued = false
            projectionReconcileGeneration += 1
        }
        const scope = currentChatScope()
        const currentChat = currentCharacter.chats?.[currentCharacter.chatPage]
        const conversationIdentity = currentConversationHandoffIdentity()
        if (activeScope !== scope || !currentChat) {
            const resetScroll = !currentChat || renderedConversationIdentity !== conversationIdentity
            resetViewport(scope, resetScroll)
            // Drafts of a conversation that was left or removed are dropped without asking.
            discardEditorDraftsExcept(currentCharacter.chaId ?? null, currentChat?.id ?? null)
            if (resetScroll) scheduleScreenRestore()
        }
        renderedConversationIdentity = conversationIdentity
        renderedStartOffset = hasConversationStart() ? 1 : 0
        renderedOwnerId = currentCharacter.chaId
        renderedConversationId = currentCharacter.chats[currentCharacter.chatPage]?.id ?? null
        if (!currentChat) {
            abortSourceLoads()
            return null
        }
        const reloadPointerMap = get(ReloadChatPointer)
        const sourceSnapshot = currentSourceSnapshot()
        const jump = currentPendingJump()
        updateLatestPair()
        const jumpRow = jump && !jump.message ? sourceSnapshot?.rowAt(jump.index) : null
        if (jump && jumpRow && sourceSnapshot) {
            // Loading this window can mount a history-dependent greeting and
            // synchronously publish another source before ensureRange returns.
            jump.sourceSnapshot = sourceSnapshot
            jump.message = cloneDeep(jumpRow.message)
        }
        const jumpTarget =
            options.jumpTarget ??
            (jump ? jump.index + (hasConversationStart() ? 1 : 0) : undefined)
        const messageCount = currentMessageCount(sourceSnapshot)
        if (
            options.jumpTarget !== undefined ||
            (initialLatestMessageCount !== null &&
                messageCount !== initialLatestMessageCount)
        )
            initialLatestFollow = false
        if (initialLatestFollow) initialLatestMessageCount = messageCount
        syncIdentityRegistration(scope, reloadPointerMap)
        const keySource = viewportKeySource(scope, sourceSnapshot)
        const preservedAnchor =
            initialLatestFollow || jump
                ? null
                : options.anchor !== undefined
                  ? options.anchor
                  : options.preserveAnchor === false
                    ? viewportAnchor
                    : (pendingSourceHandoffAnchor ??
                      pendingMissingRowsAnchor ??
                      viewportAnchor ??
                      captureDomAnchor())
        settleEditorDrafts(currentChat, sourceSnapshot)
        releaseClosedEditorPins()
        const budget = getRuntimePerformanceBudgets().chatMountedMessageBudget
        const result = buildChatViewport({
            keySource,
            budget,
            overscan: Math.min(VIEWPORT_OVERSCAN, budget - 1),
            estimatedMessageHeight: ESTIMATED_MESSAGE_HEIGHT,
            measuredHeightsByIndex: measuredHeightIndices,
            anchor: preservedAnchor,
            jumpTarget,
            pins: currentPins(currentChat, sourceSnapshot),
            ...viewportGeometry(),
        })
        viewportAnchor = result.anchor
        viewportResult = result
        syncSourcePins(result, currentChat, sourceSnapshot)
        requestMissingSourceRows(result, sourceSnapshot)
        beginViewportLayoutChange()
        renderViewportRows(
            scope,
            result,
            currentChat,
            reloadPointerMap,
            sourceSnapshot,
            result.anchor?.key,
        )
        chatBody.dataset.chatPinOverflow = String(result.pinOverflow?.count ?? 0)
        chatBody.dataset.chatMeasuredHeightCount = String(measuredHeights.size)
        chatBody.dataset.chatKeyLookupScans = String(keyLookupScans)
        correctDomAnchor(preservedAnchor)
        keepInitialLatestPosition()
        hasRenderedChat = true
        return result
    }

    function renderViewportRows(
        scope: string,
        result: ChatViewportResult,
        currentChat: character['chats'][number] | undefined,
        reloadPointerMap: Record<number, number>,
        sourceSnapshot: ConversationViewportSnapshot | null,
        preferredMountKey?: string,
    ): void {
        const currentRenderKeys = new Set<string>()
        const orderedElements: HTMLElement[] = []
        const existingGaps = new Map(
            Array.from(chatBody.children)
                .filter((element): element is HTMLElement => element instanceof HTMLElement && !!element.dataset.chatGap)
                .map((element) => [element.dataset.chatGap, element]),
        )
        const configuredPerformanceMode = DBState.db.streamingDisplayOptimizationMode ?? 'off'
        const performanceMode = currentChat?.isStreaming
            ? currentChat.activeStreamingDisplayOptimizationMode ?? configuredPerformanceMode
            : configuredPerformanceMode
        const totalMessages = currentMessageCount(sourceSnapshot)
        const startOffset = hasConversationStart() ? 1 : 0
        if (totalMessages === 0) {
            initialRowsLoading = false
            initialRowsLoadFailed = false
            pendingMissingRowsAnchor = null
        } else if (!hasMountedUsableRow && !initialRowsLoadFailed)
            initialRowsLoading = true
        const hasMissingSourceRows =
            sourceSnapshot !== null &&
            result.messageRows.some(
                (row) =>
                    row.index >= startOffset &&
                    sourceSnapshot.rowAt(row.index - startOffset) === undefined,
            )
        if (hasMissingSourceRows && hasMountedUsableRow) {
            pendingMissingRowsAnchor = result.anchor
            return
        }
        const activeStreamingIndex =
            (performanceMode !== 'off' ||
                (DBState.db.streamingThoughtMode ?? 'recent') !== 'off' ||
                DBState.db.streamingDeferDisplayProcessing) &&
            currentChat?.isStreaming
                ? totalMessages - 1
                : -1

        let rerollIndex = totalMessages - 1
        while (rerollIndex >= 0) {
            const last = sourceSnapshot?.rowAt(rerollIndex)?.message ?? messages?.[rerollIndex]
            if (!last || (!last.isComment && !last.disabled)) {
                if (last?.role !== 'char') rerollIndex = -1
                break
            }
            rerollIndex--
        }
        const globalReloadPointer = get(ReloadGUIPointer)

        for (const row of [...result.rows].reverse()) {
            if (row.kind === 'gap') {
                const gap = existingGaps.get(`${row.startIndex}:${row.endIndex}`) ?? document.createElement('div')
                gap.className = 'chat-viewport-gap'
                gap.dataset.chatGap = `${row.startIndex}:${row.endIndex}`
                gap.dataset.chatGapStart = String(row.startIndex)
                gap.dataset.chatGapEnd = String(row.endIndex)
                gap.setAttribute('aria-hidden', 'true')
                if (gap.style.height !== `${row.height}px`) gap.style.height = `${row.height}px`
                if (gap.style.flexBasis !== `${row.height}px`) gap.style.flexBasis = `${row.height}px`
                orderedElements.push(gap)
                continue
            }

            if (startOffset === 1 && row.index === 0) {
                const key = conversationStartKey(scope)
                const element = ensureElement(key, row.index)
                currentRenderKeys.add(key)
                renderConversationStart(key, element, totalMessages)
                element.classList.remove('is-latest-chat-row')
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
            const chatViewReport = {
                characterId: currentCharacter.chaId,
                conversationId: currentChat?.id,
                index,
                message,
                streaming:
                    currentChat?.isStreaming === true &&
                    index === totalMessages - 1 &&
                    message.role === 'char',
            }
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
            if (viewportRow && sourceSnapshot && activeViewportSource &&
                (previousSignature?.index !== index || sourceHandoffRuntimeKeys.has(key))) {
                const source = activeViewportSource
                untrack(() => mountInstances.get(key)?.updateViewportBinding?.({
                    viewportRow,
                    viewportSourceToken: sourceSnapshot.sourceToken,
                    captureViewportTarget: () => source.captureMessageTarget(viewportRow.key),
                    totalMessages,
                    preserveDisplay: true,
                }))
            }
            let parserProjection: BoundedLiveChatParserProjection | undefined
            let parserProjectionState: RowParserProjectionState | undefined
            if (sourceSnapshot && viewportRow && activeViewportSource && parserProjectionResolver) {
                parserProjectionState = ensureRowParserProjection(
                    key,
                    viewportRow,
                    totalMessages,
                    renderSignature,
                    sourceSnapshot,
                )
                if (!parserProjectionState.projection) {
                    markRowMountPending(key, element)
                    orderedElements.push(element)
                    continue
                }
                if (parserProjectionState.projection.kind === 'bounded') {
                    parserProjection = parserProjectionState.projection
                }
            }
            untrack(() => mountInstances.get(key)?.updatePresentation?.({
                img: (message.role === 'user' ? resolvedUserImage : resolvedCharacterImage) ?? '',
                name: message.role === 'user' ? currentUsername : currentCharacter.name,
                largePortrait: messageLargePortrait,
                bookmarked,
                role: message.role,
                messageGenerationInfo: message.generationInfo,
            }))
            const candidatePage = message.responseVariants
                ? message.responseVariants.candidates.findIndex(
                      (candidate) => candidate.id === message.responseVariants.selectedId,
                  ) + 1
                : 1
            const candidateTotal = message.responseVariants?.candidates.length ?? 1
            // The candidate position is not part of the render signature, so rows
            // refreshed in place would otherwise keep their previous position.
            untrack(() => mountInstances.get(key)?.updateCandidatePosition?.(candidatePage, candidateTotal))
            renderedMessageIds.set(key, message.chatId)
            const sourceHandoff = sourceHandoffRuntimeKeys.has(key)
            const requiresRemount =
                sourceHandoff ||
                parserProjectionState?.needsRemount === true ||
                !areChatRenderSignaturesEqual(previousSignature, renderSignature)
            const settlingThoughtPreview =
                !activeStreamingMessage &&
                previousSignature?.content === null &&
                untrack(() => mountInstances.get(key)?.hasStreamingPreview?.()) ===
                    true
            const refreshMountedDisplay =
                !sourceHandoff &&
                canRefreshChatRenderInPlace(
                    settlingThoughtPreview
                        ? { ...previousSignature, content: renderSignature.content }
                        : previousSignature,
                    renderSignature,
                ) &&
                typeof mountInstances.get(key)?.refreshMessageDisplay === 'function'

            // An open editor holds an unsaved draft, so any remount waits until it
            // closes. Source-backed rows receive their current address below.
            const deferRemountForEditor =
                requiresRemount &&
                !refreshMountedDisplay &&
                (viewportRow !== undefined || previousSignature?.index === index) &&
                mountInstances.has(key) &&
                hasActiveEditor(key)
            const preserveMountedRuntime =
                deferRemountForEditor ||
                ((sourceHandoff || parserProjectionState?.preserveMountedRuntime === true) &&
                    mountInstances.has(key) &&
                    (activeStreamingMessage ||
                        hasFocusedEditor(key) ||
                        pinReasons.get(key)?.has('playing-media') === true))
            if (requiresRemount && !preserveMountedRuntime) {
                const source = activeViewportSource
                const sourceToken = sourceSnapshot?.sourceToken
                const sourceVersion = sourceSnapshot?.version
                const sourceRowVersion = viewportRow?.sourceVersion
                const queuedMount = pendingRowMounts.get(key)
                const displayRequestOrder = queuedMount && queuedMount.parserProjectionState === parserProjectionState
                    ? queuedMount.displayRequestOrder
                    : takeRowDisplayRequestOrder(parserProjectionState)
                markRowMountPending(key, element)
                pendingRowMounts.set(key, {
                    key,
                    element,
                    priority:
                        index === totalMessages - 1
                            ? 0
                            : key === preferredMountKey || activeStreamingMessage
                              ? 1
                              : row.pinReasons.length > 0
                                ? 2
                                : 3,
                    order: refreshMountedDisplay ? -index : ++mountQueueOrder,
                    parserProjectionState,
                    displayRequestOrder,
                    isCurrent: () => {
                        if (
                            destroyed ||
                            activeScope !== scope ||
                            mountedElements.get(key) !== element ||
                            !renderKeys.has(key)
                        )
                            return false
                        if (
                            parserProjectionState &&
                            !isRowParserProjectionCurrent(key, parserProjectionState)
                        ) {
                            return false
                        }
                        if (!sourceSnapshot) return activeViewportSource === null
                        if (activeViewportSource !== source) return false
                        const snapshot = currentSourceSnapshot()
                        if (snapshot?.sourceToken !== sourceToken || snapshot.version !== sourceVersion)
                            return false
                        const currentRow = snapshot.rowAt(index)
                        return (
                            currentRow?.key === viewportRow?.key &&
                            currentRow.sourceVersion === sourceRowVersion
                        )
                    },
                    run: (settled) => {
                        if (parserProjectionState) parserProjectionState.displayStarted = true
                        if (refreshMountedDisplay) {
                            const instance = mountInstances.get(key)
                            // Exit the preview in the retained Chat before requesting
                            // its canonical render. Keep the preview visible meanwhile.
                            untrack(() =>
                                instance?.updateStreamingDisplay?.({
                                    isOptimizedStreamingMessage: activeStreamingMessage,
                                    streamingOptimizationMode: performanceMode,
                                    rawStreamingText: message.data,
                                }),
                            )

                            untrack(() =>
                                instance?.refreshMessageDisplay?.({
                                    message: message.data,
                                    index,
                                    character: simpleChar,
                                    totalMessages,
                                    parserProjection,
                                    parserAbortSignal: parserProjectionState?.controller.signal,
                                    onDisplaySettled: settled,
                                    displayRequestOrder,
                                    viewportBinding:
                                        viewportRow && source && sourceToken
                                            ? {
                                                  viewportRow,
                                                  viewportSourceToken: sourceToken,
                                                  captureViewportTarget: () =>
                                                      source.captureMessageTarget(viewportRow.key),
                                              }
                                            : undefined,
                                }),
                            )
                            renderSignatures.set(key, renderSignature)
                            if (parserProjectionState) parserProjectionState.needsRemount = false
                            chatView.rendered(key, chatViewReport)
                            return
                        }
                        releaseRowRuntimeState(key, true)
                        unmountInstance(key)
                        element.replaceChildren()
                        const restoredEditor = renderedOwnerId !== null && renderedConversationId !== null
                            ? takeEditorDraft(renderedOwnerId, renderedConversationId, index, message)
                            : null
                        const instance: ChatInstance = mount(Chat, {
                            target: element,
                            context: windowLayerContext,
                            props: {
                                restoredEditor: restoredEditor ?? undefined,
                                onEditorOpen: () => holdOpenedEditor(instance),
                                onEditorClose: () => queueProjectionReconcile(),
                                onBodyRendered: () => reportBodyRendered(instance),
                                onDisplaySettled: settled,
                                displayRequestOrder,
                                message: message.data,
                                viewportRow,
                                captureViewportTarget:
                                    viewportRow && source
                                        ? () => source.captureMessageTarget(viewportRow.key)
                                        : undefined,
                                viewportSourceToken: viewportRow ? sourceToken : undefined,
                                selectedConversationOperations,
                                bookmarked,
                                isLastMemory: false,
                                idx: index,
                                totalLength: totalMessages,
                                img:
                                    (message.role === 'user' ? resolvedUserImage : resolvedCharacterImage) ??
                                    '',
                                onReroll,
                                onNextReroll,
                                currentPage: candidatePage,
                                totalPages: candidateTotal,
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
                                parserAbortSignal: parserProjectionState?.controller.signal,
                            },
                        })
                        mountInstances.set(key, instance)
                        if (restoredEditor) holdRestoredEditor(key, element, restoredEditor)
                        else if (pendingConversationDrafts().some((draft) => draft.index === index)) queueProjectionReconcile()
                        renderSignatures.set(key, renderSignature)
                        if (parserProjectionState) {
                            parserProjectionState.needsRemount = false
                            parserProjectionState.failureCount = 0
                        }
                        sourceHandoffRuntimeKeys.delete(key)
                        completePendingMissingRowsAnchor(key)
                        chatView.rendered(key, chatViewReport)
                    },
                })
            } else {
                pendingRowMounts.delete(key)
                if (!activeRowPreparations.has(key)) clearQueuedRowHeight(element)
                const instance = mountInstances.get(key)
                let refreshedDisplay = false
                if ((sourceHandoff || preserveMountedRuntime) && viewportRow && activeViewportSource && sourceSnapshot) {
                    const source = activeViewportSource
                    instance?.updateViewportBinding?.({
                        viewportRow,
                        viewportSourceToken: sourceSnapshot.sourceToken,
                        captureViewportTarget: () => source.captureMessageTarget(viewportRow.key),
                        parserProjection,
                        totalMessages,
                    })
                }
                instance?.updateStreamingDisplay?.({
                    isOptimizedStreamingMessage: activeStreamingMessage,
                    streamingOptimizationMode: performanceMode,
                    rawStreamingText: message.data,
                })
                if (activeStreamingMessage && parserProjectionState?.needsRemount) {
                    // The row survives streaming revisions, but its previous parser
                    // lease was aborted. Publish the replacement context and signal
                    // together so the retained body can render the new text.
                    untrack(() =>
                        instance?.refreshMessageDisplay?.({
                            message: message.data,
                            index,
                            character: simpleChar,
                            totalMessages,
                            parserProjection,
                            parserAbortSignal: parserProjectionState.controller.signal,
                            onDisplaySettled: () => settlePreparedRow(key, parserProjectionState!),
                        }),
                    )
                    renderSignatures.set(key, renderSignature)
                    parserProjectionState.needsRemount = false
                    parserProjectionState.displayStarted = true
                    refreshedDisplay = true
                }
                if (
                    (deferRemountForEditor || (preserveMountedRuntime && hasActiveEditor(key))) &&
                    parserProjectionState?.needsRemount
                ) {
                    // The replacement projection aborted the retained lease, including when
                    // an open editor's row only moved. Keep the previous signature so the
                    // row still refreshes after the editor closes.
                    untrack(() =>
                        instance?.refreshMessageDisplay?.({
                            message: message.data,
                            index,
                            character: simpleChar,
                            totalMessages,
                            parserProjection,
                            parserAbortSignal: parserProjectionState.controller.signal,
                            onDisplaySettled: () => settlePreparedRow(key, parserProjectionState!),
                        }),
                    )
                    parserProjectionState.needsRemount = false
                    parserProjectionState.displayStarted = true
                    refreshedDisplay = true
                }
                if (instance) {
                    if (!refreshedDisplay && parserProjectionState && !parserProjectionState.displayStarted) settlePreparedRow(key, parserProjectionState)
                    if (refreshedDisplay) chatView.rendered(key, chatViewReport)
                    else chatView.updated(key, chatViewReport)
                }
            }

            const latest = index === totalMessages - 1
            element.classList.toggle('is-latest-chat-row', latest)
            element.classList.toggle('is-reroll-chat-row', index === rerollIndex)
            orderedElements.push(element)
        }

        reconcileChatViewportChildren(chatBody, orderedElements)
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
        renderKeys = currentRenderKeys
        for (const key of [...pendingRowMounts.keys()]) {
            if (!currentRenderKeys.has(key)) pendingRowMounts.delete(key)
        }
        startRowMountDrain()
        if (
            pendingMissingRowsAnchor &&
            mountedElements.has(pendingMissingRowsAnchor.key)
        ) {
            completePendingMissingRowsAnchor(pendingMissingRowsAnchor.key)
        }
    }

    function completePendingMissingRowsAnchor(key: string): void {
        const loadedAnchor = pendingMissingRowsAnchor
        if (!loadedAnchor || loadedAnchor.key !== key) return
        viewportAnchor = loadedAnchor
        correctDomAnchor(loadedAnchor)
        scheduleFrame(() => {
            if (pendingMissingRowsAnchor === loadedAnchor)
                pendingMissingRowsAnchor = null
        })
    }

    function markRowMountPending(key: string, element: HTMLElement): void {
        element.dataset.chatMountPending = 'true'
        if (!mountInstances.has(key)) {
            const height = measuredHeights.get(key) ?? ESTIMATED_MESSAGE_HEIGHT
            element.dataset.chatHeightPending = 'true'
            element.style.minHeight = `${height}px`
            element.style.flexBasis = `${height}px`
        }
    }

    function clearQueuedRowHeight(element: HTMLElement): void {
        element.style.removeProperty('min-height')
        element.style.removeProperty('flex-basis')
        delete element.dataset.chatMountPending
        delete element.dataset.chatHeightPending
    }

    function nextPendingRowMount(): PendingRowMount | undefined {
        let next: PendingRowMount | undefined
        for (const request of pendingRowMounts.values()) {
            if (!activeRowPreparations.has(request.key) && activeRowPreparations.size >= getRuntimePerformanceBudgets().chatActiveRowPreparations) continue
            if (
                !next ||
                request.priority < next.priority ||
                (request.priority === next.priority &&
                    request.order < next.order)
            )
                next = request
        }
        return next
    }

    function finishRowPreparation(key: string, token: object): void {
        if (activeRowPreparations.get(key) !== token) return
        activeRowPreparations.delete(key)
        if (mountDrainQueued || destroyed) return
        mountDrainQueued = true
        const generation = mountQueueGeneration
        void yieldToMainThread().then(() => {
            if (generation !== mountQueueGeneration || destroyed) return
            mountDrainQueued = false
            startRowMountDrain()
        })
    }

    function settlePreparedRow(key: string, token: object): void {
        if (activeRowPreparations.get(key) !== token) return
        const element = mountedElements.get(key)
        if (element?.dataset.chatMountPending === 'true') {
            const anchor = initialLatestFollow || currentPendingJump() ? null : captureDomAnchor()
            beginViewportLayoutChange()
            clearQueuedRowHeight(element)
            correctDomAnchor(anchor)
            keepInitialLatestPosition()
        }
        if (element && !element.dataset.chatConversationStartSignature) {
            hasMountedUsableRow = true
            initialRowsLoading = false
            initialRowsLoadFailed = false
            updateLatestPair()
        }
        settleRowMountWaiters(key, true)
        finishRowPreparation(key, token)
    }

    function startNextRowPreparation(): boolean {
        const priority = (key: string, state: RowParserProjectionState) => key === viewportAnchor?.key ? 0
            : latestPairIndices.includes(state.row.absoluteIndex) ? 1 : 2
        const candidates = activeRowPreparations.size >= getRuntimePerformanceBudgets().chatActiveRowPreparations ? []
            : [...rowParserProjections].filter(([key, state]) =>
                renderKeys.has(key) && state.preparationPending && !state.failed && isRowParserProjectionCurrent(key, state))
        candidates.sort(([leftKey, left], [rightKey, right]) =>
            priority(leftKey, left) - priority(rightKey, right) ||
            Math.abs(left.row.absoluteIndex - (viewportAnchor?.indexHint ?? 0)) - Math.abs(right.row.absoluteIndex - (viewportAnchor?.indexHint ?? 0)))
        const next = candidates[0]
        const request = nextPendingRowMount()
        if (request && (activeRowPreparations.has(request.key) || !next || request.priority < priority(...next))) {
            pendingRowMounts.delete(request.key)
            if (!request.isCurrent()) return true
            const token = request.parserProjectionState ?? request
            activeRowPreparations.set(request.key, token)
            try {
                request.run(() => settlePreparedRow(request.key, token))
            } catch (error) {
                finishRowPreparation(request.key, token)
                if (request.parserProjectionState) handleRowParserProjectionFailure(request.key, request.parserProjectionState)
                else console.error('Failed to mount chat row', error)
            }
            return true
        }
        if (!next) return false
        const [key, state] = next
        state.preparationPending = false
        activeRowPreparations.set(key, state)
        resolveRowParserProjection(key, state)
        return true
    }

    function startRowMountDrain(): void {
        if (mountDrainActive || mountDrainQueued || destroyed) return
        const queueGeneration = mountQueueGeneration
        mountDrainActive = true
        void (async () => {
            try {
                while (
                    !destroyed &&
                    queueGeneration === mountQueueGeneration
                ) {
                    let mountedThisTask = 0
                    while (mountedThisTask < CHAT_MOUNT_BATCH_SIZE) {
                        if (!startNextRowPreparation()) return
                        mountedThisTask += 1
                    }
                    await yieldToMainThread()
                }
            } finally {
                mountDrainActive = false
            }
        })()
    }

    function waitForRowMount(
        key: string,
        generation: number,
    ): Promise<boolean> {
        if (mountInstances.has(key) && !activeRowPreparations.has(key)) return Promise.resolve(true)
        const projection = rowParserProjections.get(key)
        if (
            projection?.failed &&
            projection.failureCount >= PARSER_PROJECTION_MAX_ATTEMPTS
        )
            return Promise.resolve(false)
        if (generation !== navigationGeneration || destroyed)
            return Promise.resolve(false)
        return new Promise<boolean>((resolve) => {
            const waiters =
                rowMountWaiters.get(key) ?? new Set<RowMountWaiter>()
            waiters.add({ navigationGeneration: generation, resolve })
            rowMountWaiters.set(key, waiters)
        })
    }

    function settleRowMountWaiters(key: string, mounted: boolean): void {
        const waiters = rowMountWaiters.get(key)
        if (!waiters) return
        rowMountWaiters.delete(key)
        for (const waiter of waiters) {
            waiter.resolve(
                mounted &&
                    waiter.navigationGeneration === navigationGeneration &&
                    mountInstances.has(key),
            )
        }
    }

    function cancelStaleRowMountWaiters(): void {
        for (const [key, waiters] of rowMountWaiters) {
            for (const waiter of [...waiters]) {
                if (
                    waiter.navigationGeneration === navigationGeneration &&
                    !destroyed
                )
                    continue
                waiters.delete(waiter)
                waiter.resolve(false)
            }
            if (waiters.size === 0) rowMountWaiters.delete(key)
        }
    }

    function clearPendingRowMounts(): void {
        mountDrainQueued = false
        mountQueueGeneration += 1
        pendingRowMounts.clear()
        for (const key of [...rowMountWaiters.keys()])
            settleRowMountWaiters(key, false)
    }

    function remapMountedSourceRows(nextSnapshot: ConversationViewportSnapshot): void {
        const remapped: Array<{ previousKey: string; nextKey: string }> = []
        for (const [previousKey, element] of mountedElements) {
            const absoluteIndex = Number(element.dataset.chatIndex)
            if (!Number.isSafeInteger(absoluteIndex) || absoluteIndex < 0) continue
            const nextKey = nextSnapshot.keyAt(absoluteIndex)
            if (nextKey === undefined) continue
            remapped.push({ previousKey, nextKey })
        }
        for (const key of [...rowParserProjections.keys()]) {
            releaseRowParserProjection(key)
        }
        for (const { previousKey, nextKey } of remapped) {
            if (previousKey === nextKey) continue
            moveKeyedEntry(mountedElements, previousKey, nextKey)
            moveKeyedEntry(mountInstances, previousKey, nextKey)
            moveKeyedEntry(renderSignatures, previousKey, nextKey)
            moveKeyedEntry(pinReasons, previousKey, nextKey)
            moveKeyedEntry(playingMedia, previousKey, nextKey)
            moveKeyedEntry(measuredHeights, previousKey, nextKey)
            moveKeyedEntry(measuredHeightRecency, previousKey, nextKey)
            moveKeyedEntry(measuredHeightIndexByKey, previousKey, nextKey)
            if (renderKeys.delete(previousKey)) renderKeys.add(nextKey)
            sourceHandoffRuntimeKeys.delete(previousKey)
            sourceHandoffRuntimeKeys.add(nextKey)
            const element = mountedElements.get(nextKey)
            if (element) element.dataset.chatRenderKey = nextKey
            chatView.moved(previousKey, nextKey)
        }
    }

    function moveKeyedEntry<T>(
        values: Map<string, T>,
        previousKey: string,
        nextKey: string,
    ): void {
        const value = values.get(previousKey)
        if (value === undefined) return
        values.delete(previousKey)
        values.set(nextKey, value)
    }

    function ensureElement(key: string, viewportIndex: number): HTMLElement {
        let element = mountedElements.get(key)
        if (!element) {
            element = document.createElement('div')
            element.dataset.chatRenderKey = key
            element.classList.add('chat-message-container')
            mountedElements.set(key, element)
            resizeObserver?.observe(element, { box: 'border-box' })
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
        const historySnapshot = activeViewportSource?.snapshot()
        const signature = JSON.stringify([
            character.chaId,
            currentChat.id,
            currentChat.fmIndex,
            character.firstMessage,
            character.alternateGreetings,
            character.creatorNotes,
            character.removedQuotes,
            character.largePortrait,
            showAiWarning,
            totalMessages,
            parserCharacterStamp,
            get(ReloadGUIPointer),
            [historySnapshot?.storeRevision, historySnapshot?.historyVersion ?? 0],
        ])
        const previousSignature = element.dataset.chatConversationStartSignature
        if (previousSignature && previousSignature !== signature && mountInstances.has(key)) {
            const previousInputs = JSON.parse(previousSignature)
            const nextInputs = JSON.parse(signature)
            // Recheck history admission on reload or history edits without discarding the greeting.
            previousInputs[9] = nextInputs[9]
            previousInputs[11] = nextInputs[11]
            previousInputs[12] = nextInputs[12]
            if (JSON.stringify(previousInputs) === signature) {
                element.dataset.chatConversationStartSignature = signature
                const forced = JSON.parse(previousSignature)[11] !== nextInputs[11]
                untrack(() => mountInstances.get(key)?.refreshConversationStartParser?.(totalMessages, historySnapshot, forced))
                return
            }
        }
        if (
            element.dataset.chatConversationStartSignature === signature &&
            mountInstances.has(key)
        ) {
            mountInstances.get(key)?.updateConversationStartPresentation?.({
                resolvedImage: resolvedCharacterImage ?? '',
            })
            return
        }
        if (element.dataset.chatConversationStartSignature === signature && pendingRowMounts.has(key)) return
        unmountInstance(key)
        const previous = activeRowPreparations.get(key)
        if (previous) finishRowPreparation(key, previous)
        element.dataset.chatConversationStartSignature = signature
        const displayRequestOrder = pendingRowMounts.get(key)?.displayRequestOrder ?? nextDisplayListenerOrder()
        markRowMountPending(key, element)
        const request: PendingRowMount = {
            key, element,
            priority: viewportAnchor?.key === key ? 0 : 2,
            order: ++mountQueueOrder,
            displayRequestOrder,
            isCurrent: () => mountedElements.get(key) === element && element.dataset.chatConversationStartSignature === signature,
            run(settled) {
                element.replaceChildren()
                const instance = mount(ChatConversationStart, {
                    target: element,
                    context: windowLayerContext,
                    props: {
                        currentCharacter: character,
                        resolvedImage: resolvedCharacterImage ?? '',
                        showAiWarning,
                        totalMessages,
                        historySnapshot,
                        onReroll: onFirstMessageReroll,
                        unReroll: unFirstMessageReroll,
                        onRemoveCreatorQuote,
                        acquireConversationStartParserLease,
                        selectedConversationOperations,
                        onDisplaySettled: settled,
                        displayRequestOrder,
                    },
                })
                mountInstances.set(key, instance)
            },
        }
        pendingRowMounts.set(key, request)
    }

    function unmountInstance(key: string): void {
        const instance = mountInstances.get(key)
        if (!instance) return
        keepEditorDraftOf(key, instance)
        unmount(instance)
        mountInstances.delete(key)
    }

    // Every row teardown passes here, so an open editor's draft is kept for the
    // next mount of the same message instead of being lost with the row.
    function keepEditorDraftOf(key: string, instance: ChatInstance): void {
        const captured = untrack(() => instance.captureEditorDraft?.())
        if (!captured || renderedOwnerId === null || renderedConversationId === null) return
        const focused = document.activeElement
        const caret = focused instanceof HTMLTextAreaElement && mountedElements.get(key)?.contains(focused)
            ? [focused.selectionStart, focused.selectionEnd] as const
            : undefined
        keepEditorDraft(renderedOwnerId, renderedConversationId, { ...captured, caret })
    }

    const restoredDrafts = new WeakSet<ChatEditorDraft>()

    function holdRestoredEditor(key: string, element: HTMLElement, draft: ChatEditorDraft): void {
        restoredDrafts.add(draft)
        const reasons = pinReasons.get(key) ?? new Set<ChatViewportPinReason>()
        reasons.add('editor')
        pinReasons.set(key, reasons)
        blurredEditorPins.add(key)
        if (!draft.caret) return
        const caret = draft.caret
        void domSettled().then(() => {
            const focused = document.activeElement
            if (focused && focused !== document.body && !element.contains(focused)) return
            const editor = element.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')
            if (!editor) return
            editor.focus({ preventScroll: true })
            editor.setSelectionRange(caret[0], caret[1])
        })
    }

    // An editor opened without focus in the row, such as through the popup
    // hotkey, still keeps its row until it closes.
    function holdOpenedEditor(instance: ChatInstance): void {
        const key = [...mountInstances].find(([, value]) => value === instance)?.[0]
        if (key === undefined) return
        blurredEditorPins.add(key)
        addPin(key, 'editor')
    }

    function reportBodyRendered(instance: ChatInstance): void {
        const key = [...mountInstances].find(([, value]) => value === instance)?.[0]
        if (key !== undefined) chatView.contentRendered(key)
    }

    function pendingConversationDrafts(): readonly ChatEditorDraft[] {
        const conversationId = currentCharacter.chats?.[currentCharacter.chatPage]?.id
        return conversationId ? pendingEditorDrafts(currentCharacter.chaId, conversationId) : []
    }

    const relocatingDrafts = new WeakSet<ChatEditorDraft>()

    // Keeps each kept draft's row loaded, hands the draft to a mounted row that
    // shows exactly its message, and otherwise looks the message up by its id.
    function settleEditorDrafts(
        currentChat: character['chats'][number],
        sourceSnapshot: ConversationViewportSnapshot | null,
    ): void {
        const conversationId = currentChat.id
        if (!conversationId) return
        for (const draft of pendingEditorDrafts(currentCharacter.chaId, conversationId)) {
            if (relocatingDrafts.has(draft)) continue
            const key = currentMessageKey(draft.index, sourceSnapshot)
            if (key === undefined) {
                relocateEditorDraft(conversationId, draft)
                continue
            }
            const reasons = pinReasons.get(key) ?? new Set<ChatViewportPinReason>()
            reasons.add('editor')
            pinReasons.set(key, reasons)
            blurredEditorPins.add(key)
            const message = sourceSnapshot ? sourceSnapshot.rowAt(draft.index)?.message : messages?.[draft.index]
            if (!message) continue
            if (!isEqual(message, draft.evidence)) {
                relocateEditorDraft(conversationId, draft)
                continue
            }
            const instance = mountInstances.get(key)
            const element = mountedElements.get(key)
            if (!instance || !element || pendingRowMounts.has(key)) continue
            if (hasActiveEditor(key)) {
                discardEditorDraft(currentCharacter.chaId, conversationId, draft)
                continue
            }
            const taken = takeEditorDraft(currentCharacter.chaId, conversationId, draft.index, message)
            if (!taken) continue
            untrack(() => instance.restoreEditor?.(taken))
            holdRestoredEditor(key, element, taken)
        }
    }

    // A draft whose index no longer holds its message follows the message only
    // when exactly one message carries its id and still equals what was edited.
    function relocateEditorDraft(conversationId: string, draft: ChatEditorDraft): void {
        const ownerId = currentCharacter.chaId
        const messageId = draft.evidence.chatId
        if (!messageId) {
            discardEditorDraft(ownerId, conversationId, draft)
            return
        }
        relocatingDrafts.add(draft)
        const generation = navigationGeneration
        const settle = (found: { absoluteIndex: number; message: Readonly<Message> } | null) => {
            relocatingDrafts.delete(draft)
            if (destroyed || generation !== navigationGeneration) return
            if (!pendingEditorDrafts(ownerId, conversationId).includes(draft)) return
            if (!found || !isEqual(found.message, draft.evidence)) {
                discardEditorDraft(ownerId, conversationId, draft)
                return
            }
            draft.index = found.absoluteIndex
            reconcileViewport()
        }
        let lookup: Promise<{ absoluteIndex: number; message: Readonly<Message> } | null>
        if (activeViewportSource) {
            lookup = selectedConversationOperations?.findUniqueMessage(messageId) ?? Promise.resolve(null)
        } else {
            const indices = (messages ?? []).flatMap((message, index) => message.chatId === messageId ? [index] : [])
            lookup = Promise.resolve(indices.length === 1 ? { absoluteIndex: indices[0], message: messages![indices[0]] } : null)
        }
        // Settles after the current reconcile, which is still running.
        void lookup.then(settle, () => settle(null))
    }

    function retainScreenAnchor(): void {
        if (!renderedOwnerId || !renderedConversationId || mountedElements.size === 0) return
        if (currentCharacter.chaId === renderedOwnerId &&
            !currentCharacter.chats?.some((chat) => chat.id === renderedConversationId)) {
            chatScreenState.remove({ characterId: renderedOwnerId, conversationId: renderedConversationId })
            return
        }
        const anchor = captureDomAnchor()
        if (!anchor) return
        chatScreenState.writeAnchor({ characterId: renderedOwnerId, conversationId: renderedConversationId }, {
            index: Math.max(0, anchor.indexHint - renderedStartOffset),
            messageId: renderedMessageIds.get(anchor.key),
            relativeOffset: anchor.relativeOffset,
            latest: initialLatestFollow || (scrollContainer !== null &&
                scrollContainer.scrollHeight - scrollContainer.scrollTop - scrollContainer.clientHeight < 24),
        })
    }

    function scheduleScreenRestore(): void {
        if (pendingConversationDrafts().length > 0) {
            scheduleEditorDraftJump()
            return
        }
        const characterId = currentCharacter.chaId
        const conversationId = currentCharacter.chats[currentCharacter.chatPage]?.id
        if (!characterId || !conversationId) return
        const anchor = chatScreenState.readAnchor({ characterId, conversationId })
        if (!anchor || anchor.latest) return
        const generation = navigationGeneration
        void domSettled().then(async () => {
            const current = () => !destroyed && generation === navigationGeneration &&
                currentCharacter.chaId === characterId &&
                currentCharacter.chats[currentCharacter.chatPage]?.id === conversationId
            if (!current()) return
            let index = anchor.index
            if (anchor.messageId) {
                if (activeViewportSource && selectedConversationOperations) {
                    const found = await selectedConversationOperations.findUniqueMessage(anchor.messageId).catch(() => null)
                    if (!current()) return
                    if (found) index = found.absoluteIndex
                } else if (!activeViewportSource) {
                    const matches = (messages ?? []).flatMap((message, index) => message.chatId === anchor.messageId ? [index] : [])
                    if (matches.length === 1) index = matches[0]
                }
            }
            index = Math.min(index, currentMessageCount() - 1)
            if (index < 0 || !current()) return
            if (!await jumpTo(index)) return
            if (destroyed || currentCharacter.chaId !== characterId ||
                currentCharacter.chats[currentCharacter.chatPage]?.id !== conversationId) return
            const key = currentMessageKey(index)
            if (!key) return
            viewportAnchor = { key, indexHint: index + (hasConversationStart() ? 1 : 0), relativeOffset: anchor.relativeOffset }
            correctDomAnchor(viewportAnchor)
        })
    }

    // An older message with a kept draft takes precedence over its saved viewport.
    function scheduleEditorDraftJump(): void {
        const draft = pendingConversationDrafts()[0]
        if (!draft) return
        const generation = navigationGeneration
        void domSettled().then(() => {
            if (destroyed || generation !== navigationGeneration) return
            // The pinned row may already have taken the draft; a discarded one is not followed.
            if (!restoredDrafts.has(draft) && !pendingConversationDrafts().includes(draft)) return
            void jumpTo(draft.index)
        })
    }

    function removeMountedRow(key: string): void {
        const preparation = activeRowPreparations.get(key)
        if (preparation) finishRowPreparation(key, preparation)
        pendingRowMounts.delete(key)
        settleRowMountWaiters(key, false)
        releaseRowRuntimeState(key)
        unmountInstance(key)
        const element = mountedElements.get(key)
        if (element) {
            resizeObserver?.unobserve(element)
            element.remove()
            mountedElements.delete(key)
        }
        renderSignatures.delete(key)
        renderedMessageIds.delete(key)
        sourceHandoffRuntimeKeys.delete(key)
        chatView.removed(key)
    }

    function releaseRowRuntimeState(key: string, preserveParserProjection = false): void {
        if (!preserveParserProjection) releaseRowParserProjection(key)
        const media = playingMedia.get(key)
        playingMedia.delete(key)
        // A remount can remove the focused control before focusout runs. Mirror
        // its released UI pin to the source even when no later input arrives.
        blurredEditorPins.delete(key)
        if (pinReasons.delete(key)) queueProjectionReconcile()
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
            && !existing.controller.signal.aborted
            && existing.source === source
            && existing.sourceToken === sourceSnapshot.sourceToken
            && existing.sourceVersion === sourceSnapshot.version
            && !hasCbsHistoryChanges(sourceSnapshot, existing.historyVersion, existing.dependencies)
            && existing.row.key === row.key
            && existing.row.absoluteIndex === row.absoluteIndex
            && existing.row.sourceVersion === row.sourceVersion
            && areChatRenderSignaturesEqual(existing.renderSignature, renderSignature)
        ) {
            if (existing.displayStarted && !activeRowPreparations.has(key)) existing.historyVersion = sourceSnapshot.historyVersion
            return existing
        }

        const preserveMountedRuntime = sourceHandoffRuntimeKeys.has(key) || (
            existing?.source === source && (
                existing.controller.signal.aborted
                || existing.sourceToken !== sourceSnapshot.sourceToken
                || existing.sourceVersion !== sourceSnapshot.version
                || existing.historyVersion !== sourceSnapshot.historyVersion
                || existing.row.key !== row.key
                || existing.row.absoluteIndex !== row.absoluteIndex
                || existing.row.sourceVersion !== row.sourceVersion
                || existing.totalMessages !== totalMessages
            )
        )
        releaseRowParserProjection(key)
        const controller = new AbortController()
        const dependencies = new CbsHistoryDependencies()
        dependencies.range(row.absoluteIndex, row.absoluteIndex + 1)
        const state: RowParserProjectionState = {
            dependencies,
            controller,
            source,
            sourceToken: sourceSnapshot.sourceToken,
            sourceVersion: sourceSnapshot.version,
            historyVersion: sourceSnapshot.historyVersion,
            row,
            totalMessages,
            renderSignature,
            projection: null,
            preparationPending: true,
            displayStarted: false,
            needsRemount: mountInstances.has(key),
            preserveMountedRuntime,
            failed: false,
            failureCount: 0,
            retryTimer: null,
            displayRequestOrder: nextDisplayListenerOrder(),
        }
        rowParserProjections.set(key, state)
        return state
    }

    // A row waits for its projection before it mounts; its display was requested when the wait began.
    function takeRowDisplayRequestOrder(state: RowParserProjectionState | undefined): number {
        const order = state?.displayRequestOrder
        if (state) state.displayRequestOrder = undefined
        return order ?? nextDisplayListenerOrder()
    }

    function resolveRowParserProjection(
        key: string,
        state: RowParserProjectionState,
    ): void {
        void parserProjectionResolver!.resolve({
            dependencies: state.dependencies,
            row: state.row,
            totalMessages: state.totalMessages,
            signal: state.controller.signal,
            isCurrent: () => isRowParserProjectionCurrent(key, state),
            onStale: () => handleRowParserProjectionStale(key, state),
        }).then((projection) => {
            if (projection.kind === 'complete') state.dependencies.all = true
            if (!isRowParserProjectionCurrent(key, state)) {
                if (projection.kind === 'complete') projection.release()
                return
            }
            state.projection = projection
            queueProjectionReconcile()
        }).catch(() => {
            handleRowParserProjectionFailure(key, state)
        })
    }

    function queueProjectionReconcile(): void {
        if (projectionReconcileQueued || destroyed) return
        projectionReconcileQueued = true
        const generation = projectionReconcileGeneration
        void yieldToMainThread().then(() => {
            if (
                destroyed
                || generation !== projectionReconcileGeneration
                || !projectionReconcileQueued
            ) return
            projectionReconcileQueued = false
            reconcileViewport()
        })
    }

    function handleRowParserProjectionFailure(
        key: string,
        state: RowParserProjectionState,
    ): void {
        finishRowPreparation(key, state)
        releaseResolvedRowParserProjection(state)
        if (!isRowParserProjectionCurrent(key, state)) {
            if (rowParserProjections.get(key) === state && !state.controller.signal.aborted) queueProjectionReconcile()
            return
        }
        if (
            state.retryTimer !== null
        )
            return
        state.failed = true
        state.failureCount += 1
        if (state.failureCount >= PARSER_PROJECTION_MAX_ATTEMPTS) {
            parserProjectionLoadFailed = true
            if (!hasMountedUsableRow) {
                initialRowsLoading = false
                initialRowsLoadFailed = true
            }
            settleRowMountWaiters(key, false)
            return
        }
        state.retryTimer = setTimeout(() => {
            if (!isRowParserProjectionCurrent(key, state)) return
            state.retryTimer = null
            state.failed = false
            state.needsRemount = mountInstances.has(key)
            state.preparationPending = true
            startRowMountDrain()
        }, PARSER_PROJECTION_RETRY_DELAY_MS)
    }

    // A later CBS pass read history that changed before it registered the read.
    // Abort that render and prepare the row again, as for a covered history change.
    function handleRowParserProjectionStale(
        key: string,
        state: RowParserProjectionState,
    ): void {
        if (rowParserProjections.get(key) !== state || state.controller.signal.aborted) return
        state.controller.abort()
        queueProjectionReconcile()
    }

    function isRowParserProjectionCurrent(
        key: string,
        state: RowParserProjectionState,
    ): boolean {
        if (
            state.controller.signal.aborted
            || rowParserProjections.get(key) !== state
            || activeViewportSource !== state.source
        ) return false
        const snapshot = currentSourceSnapshot()
        if (
            !snapshot
            || snapshot.sourceToken !== state.sourceToken
            || snapshot.version !== state.sourceVersion
            || hasCbsHistoryChanges(snapshot, state.historyVersion, state.dependencies)
        ) return false
        const currentRow = snapshot.rowAt(state.row.absoluteIndex)
        return currentRow?.key === state.row.key
            && currentRow.sourceVersion === state.row.sourceVersion
    }

    function releaseRowParserProjection(key: string): void {
        const state = rowParserProjections.get(key)
        if (!state) return
        rowParserProjections.delete(key)
        finishRowPreparation(key, state)
        state.controller.abort()
        if (state.retryTimer !== null) clearTimeout(state.retryTimer)
        releaseResolvedRowParserProjection(state)
        parserProjectionLoadFailed = [...rowParserProjections.values()].some(
            (projection) =>
                projection.failed &&
                projection.failureCount >= PARSER_PROJECTION_MAX_ATTEMPTS,
        )
    }

    function releaseResolvedRowParserProjection(state: RowParserProjectionState): void {
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

    function hasActiveEditor(key: string): boolean {
        return untrack(() => mountInstances.get(key)?.hasActiveEditor?.()) === true
    }

    function hasFocusedEditor(key: string): boolean {
        if (hasActiveEditor(key)) return true
        if (!pinReasons.get(key)?.has('editor')) return false
        const focused = document.activeElement
        if (
            !(focused instanceof HTMLElement) ||
            !mountedElements.get(key)?.contains(focused)
        )
            return false
        if (focused instanceof HTMLTextAreaElement) return true
        if (focused instanceof HTMLInputElement) {
            return ![
                'button',
                'submit',
                'reset',
                'checkbox',
                'radio',
                'file',
                'image',
                'range',
                'color',
                'hidden',
            ].includes(focused.type)
        }
        const editable = focused
            .closest('[contenteditable]')
            ?.getAttribute('contenteditable')
        return (
            editable === '' || editable === 'true' || editable === 'plaintext-only'
        )
    }

    function handleFocusIn(event: FocusEvent): void {
        // All focused controls keep their row resident. Only an actual editor
        // may postpone a changed message; a bot button must display its result.
        const key = rowKeyFromEvent(event)
        if (!key) return
        blurredEditorPins.delete(key)
        addPin(key, 'editor')
    }

    function handleFocusOut(event: FocusEvent): void {
        const key = rowKeyFromEvent(event)
        if (!key) return
        const row = mountedElements.get(key)
        if (event.relatedTarget instanceof Node && row?.contains(event.relatedTarget)) return
        queueMicrotask(() => {
            if (row && document.activeElement instanceof Node && row.contains(document.activeElement)) return
            // An open editor keeps its row when focus leaves it, for example when an
            // input fence makes the page inert, so its draft survives source handoffs.
            if (hasActiveEditor(key)) {
                blurredEditorPins.add(key)
                return
            }
            removePin(key, 'editor')
        })
    }

    function releaseClosedEditorPins(): void {
        const draftKeys = new Set(pendingConversationDrafts().map((draft) => currentMessageKey(draft.index)))
        for (const key of blurredEditorPins) {
            if (hasActiveEditor(key) || draftKeys.has(key)) continue
            blurredEditorPins.delete(key)
            const row = mountedElements.get(key)
            if (row && document.activeElement instanceof Node && row.contains(document.activeElement)) continue
            const reasons = pinReasons.get(key)
            if (!reasons?.delete('editor')) continue
            if (reasons.size === 0) pinReasons.delete(key)
        }
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
        finishViewportLayoutChange?.()
        projectionReconcileGeneration += 1
        projectionReconcileQueued = false
        clearPendingRowMounts()
        if (typeof cancelAnimationFrame === 'function') {
            for (const frame of animationFrames) cancelAnimationFrame(frame)
        }
        animationFrames.clear()
        suppressScroll = false
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

    function refreshViewportHeights(): void {
        if (!viewportResult || !chatBody) return
        if (scrollContainer && scrollContainer.clientHeight > 0) {
            const sourceSnapshot = currentSourceSnapshot()
            const budget = getRuntimePerformanceBudgets().chatMountedMessageBudget
            const next = buildChatViewport({
                keySource: viewportKeySource(currentChatScope(), sourceSnapshot),
                budget,
                overscan: VIEWPORT_OVERSCAN,
                estimatedMessageHeight: ESTIMATED_MESSAGE_HEIGHT,
                measuredHeightsByIndex: measuredHeightIndices,
                anchor: initialLatestFollow ? null : viewportAnchor,
                pins: currentPins(currentCharacter.chats[currentCharacter.chatPage], sourceSnapshot),
                ...viewportGeometry(),
            })
            if (next.messageRows.length !== viewportResult.messageRows.length ||
                next.messageRows.some((row, index) => row.key !== viewportResult!.messageRows[index]?.key)) {
                reconcileViewport({ anchor: initialLatestFollow ? null : captureDomAnchor() })
                return
            }
        }
        for (const gap of Array.from(chatBody.children)) {
            if (!(gap instanceof HTMLElement) || !gap.dataset.chatGap) continue
            const height = measureChatViewportRange(
                Number(gap.dataset.chatGapStart), Number(gap.dataset.chatGapEnd),
                ESTIMATED_MESSAGE_HEIGHT, measuredHeightIndices,
            )
            if (gap.style.height !== `${height}px`) gap.style.height = `${height}px`
            if (gap.style.flexBasis !== `${height}px`) gap.style.flexBasis = `${height}px`
        }
        keepInitialLatestPosition()
    }

    function viewportGeometry() {
        const budgets = getRuntimePerformanceBudgets()
        return {
            viewportHeight: scrollContainer?.clientHeight ?? 0,
            preparationScreens: budgets.chatPreparationScreens,
            retentionScreens: budgets.chatRetentionScreens,
            retainedIndices: [...mountInstances.keys()].flatMap((key) => {
                const index = Number(mountedElements.get(key)?.dataset.chatViewportIndex)
                return Number.isInteger(index) ? [index] : []
            }),
        }
    }

    function handleResize(entries: ResizeObserverEntry[]): void {
        let changed = false
        for (const entry of entries) {
            const element = entry.target as HTMLElement
            if (element === scrollContainer) {
                const width = element.clientWidth
                const height = element.clientHeight
                if (width !== containerWidth || height !== containerHeight) changed = true
                if (width !== containerWidth) {
                    // Retain old gap estimates while mounted rows report their new flow sizes.
                    for (const row of mountedElements.values()) {
                        resizeObserver?.unobserve(row)
                        resizeObserver?.observe(row, { box: 'border-box' })
                    }
                }
                containerWidth = width
                containerHeight = height
                continue
            }
            const key = element.dataset.chatRenderKey
            const box = entry.borderBoxSize?.[0]
            const vertical = box && /^(vertical|sideways)/.test(getComputedStyle(element).writingMode)
            const height = box
                ? (vertical ? box.inlineSize : box.blockSize)
                : element.getBoundingClientRect().height || entry.contentRect.height
            if (!key || !Number.isFinite(height) || height < 0) continue
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
        if (scheduledReconcileFrame !== null) return
        scheduledReconcileFrame = scheduleFrame(() => {
            scheduledReconcileFrame = null
            refreshViewportHeights()
        })
    }

    function keepInitialLatestPosition(): void {
        if (!initialLatestFollow || !scrollContainer) return
        if (scrollContainer.scrollTop !== 0) scrollContainer.scrollTop = 0
        lastScrollTop = 0
    }

    function handleUserScrollIntent(event: Event): void {
        if (event.type === 'wheel' && (event as WheelEvent).deltaY === 0) return
        if (event.type === 'pointerdown' && event.target !== scrollContainer) return
        if (event.type === 'keydown') {
            const keyboard = event as KeyboardEvent
            if (
                ![
                    'ArrowUp',
                    'ArrowDown',
                    'PageUp',
                    'PageDown',
                    'Home',
                    'End',
                    ' ',
                    'Tab',
                ].includes(keyboard.key)
            )
                return
            if (
                keyboard.key !== 'Tab' &&
                event.target instanceof Element &&
                event.target.closest(
                    'input, textarea, [contenteditable]:not([contenteditable="false"])',
                )
            )
                return
        }
        userScrollIntentAt = performance.now()
        finishViewportLayoutChange?.()
        if (event.type === 'pointerdown') scrollPointerHeld = true
        positionGeneration += 1
        initialLatestFollow = false
        if (currentPendingJump()) {
            pendingJump = null
            navigationGeneration += 1
            cancelStaleRowMountWaiters()
        }
        suppressScroll = false
        pendingSourceHandoffAnchor = null
        pendingMissingRowsAnchor = null
        pendingSourceWasAtBottom = null
        if (autoScrollTimer && !DBState.db.alwaysScrollToNewMessage) {
            clearTimeout(autoScrollTimer)
            autoScrollTimer = null
        }
        viewportAnchor = captureDomAnchor()
    }

    function releaseScrollPointer(): void {
        scrollPointerHeld = false
    }

    function handleScroll(): void {
        if (!scrollContainer || suppressScroll || finishViewportLayoutChange || !viewportResult) return
        // Layout and browser scroll anchoring also emit scroll events. Only input
        // or an explicit jump transfers initial positioning to a history anchor.
        if (initialLatestFollow) {
            keepInitialLatestPosition()
            return
        }
        if (currentPendingJump()) return
        const currentTop = scrollContainer.scrollTop
        const movingOlder = currentTop < lastScrollTop
        const movingNewer = currentTop > lastScrollTop
        lastScrollTop = currentTop
        if (!movingOlder && !movingNewer) return
        // Browser scroll anchoring also moves the position while rows grow.
        if (scrollPointerHeld || performance.now() - userScrollIntentAt < USER_SCROLL_INTENT_MS)
            onScrollMove?.()
        positionGeneration += 1
        pendingSourceHandoffAnchor = null
        pendingMissingRowsAnchor = null
        pendingSourceWasAtBottom = null
        viewportAnchor = captureDomAnchor()
        const containerRect = scrollContainer.getBoundingClientRect()
        const gaps = [...chatBody.querySelectorAll<HTMLElement>('[data-chat-gap]')]
        const visibleGap = gaps.find((gap) => {
            const rect = gap.getBoundingClientRect()
            return (
                rect.bottom >= containerRect.top && rect.top <= containerRect.bottom
            )
        })
        if (!visibleGap) {
            refreshViewportHeights()
            return
        }
        const start = Number(visibleGap.dataset.chatGapStart)
        const end = Number(visibleGap.dataset.chatGapEnd)
        const gapRect = visibleGap.getBoundingClientRect()
        const distance = Math.max(0, Math.min(gapRect.height,
            movingOlder ? gapRect.bottom - containerRect.top : containerRect.bottom - gapRect.top,
        ))
        const { index: target, traversed, height } = locateChatViewportOffset(
            start, end, distance, movingOlder, ESTIMATED_MESSAGE_HEIGHT, measuredHeightIndices,
        )
        const relativeOffset = movingOlder
            ? gapRect.bottom - traversed - height - containerRect.top
            : gapRect.top + traversed - containerRect.top
        const keySource = viewportKeySource(currentChatScope())
        if (!Number.isInteger(target) || target < 0 || target >= keySource.length)
            return
        const key = keySource.keyAt(target)
        if (key === undefined) return
        viewportAnchor = {
            key,
            indexHint: target,
            relativeOffset,
        }
        reconcileViewport({ anchor: viewportAnchor })
    }

    function isMountedKeyAtBottom(key: string): boolean {
        if (!scrollContainer) return true
        const element = mountedElements.get(key)
        if (!element) return false
        return element.getBoundingClientRect().top <= scrollContainer.getBoundingClientRect().bottom + 100
    }

    export function isAtBottom(): boolean {
        return checkIfAtBottom()
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
        const generation = navigationGeneration
        await domSettled()
        if (destroyed || generation !== navigationGeneration) return
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

    function currentPendingJump() {
        return pendingJump?.generation === navigationGeneration &&
            pendingJump.conversationIdentity ===
                currentConversationHandoffIdentity()
            ? pendingJump
            : null
    }

    function isPendingJumpSourceCurrent(
        jump: NonNullable<typeof pendingJump>,
    ): boolean {
        if (!jump.sourceSnapshot) return true
        const snapshot = currentSourceSnapshot()
        return (
            snapshot?.sourceToken === jump.sourceSnapshot.sourceToken &&
            snapshot.version === jump.sourceSnapshot.version &&
            snapshot.totalMessages === jump.sourceSnapshot.totalMessages &&
            snapshot.storeRevision === jump.sourceSnapshot.storeRevision
        )
    }

    export async function jumpTo(
        index: number,
        options: ChatViewportJumpOptions = {},
    ): Promise<boolean> {
        const totalMessages = currentMessageCount()
        if (!Number.isInteger(index) || index < 0 || index >= totalMessages)
            return false
        initialLatestFollow = false
        navigationGeneration += 1
        positionGeneration += 1
        suppressScroll = false
        pendingSourceHandoffAnchor = null
        pendingMissingRowsAnchor = null
        pendingSourceWasAtBottom = null
        if (autoScrollTimer) clearTimeout(autoScrollTimer)
        autoScrollTimer = null
        const jump = {
            generation: navigationGeneration,
            conversationIdentity: currentConversationHandoffIdentity(),
            index,
            sourceSnapshot: null as ConversationViewportSnapshot | null,
            message: null as Readonly<Message> | null,
        }
        pendingJump = jump
        cancelStaleRowMountWaiters()
        try {
            while (currentPendingJump() === jump) {
                const generation = jump.generation
                const scope = currentChatScope()
                const source = activeViewportSource
                const sourceSnapshot = currentSourceSnapshot()
                if (!isPendingJumpSourceCurrent(jump)) return false
                if (source && sourceSnapshot) {
                    const budget =
                        getRuntimePerformanceBudgets().chatMountedMessageBudget
                    const startIndex = Math.max(
                        0,
                        index - Math.min(VIEWPORT_OVERSCAN, budget - 1),
                    )
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
                        await domSettled()
                    } catch {
                        if (
                            currentPendingJump() === jump &&
                            generation !== jump.generation
                        )
                            continue
                        return false
                    } finally {
                        if (sourceLoads.get(loadKey) === controller)
                            sourceLoads.delete(loadKey)
                    }
                    if (
                        currentPendingJump() === jump &&
                        generation !== jump.generation
                    )
                        continue
                    const currentSnapshot = currentSourceSnapshot()
                    if (
                        controller.signal.aborted ||
                        generation !== navigationGeneration ||
                        source !== activeViewportSource ||
                        currentSnapshot?.sourceToken !==
                            sourceSnapshot.sourceToken ||
                        currentSnapshot.version !== sourceSnapshot.version
                    )
                        return false
                    const row = currentSnapshot.rowAt(index)
                    if (!row) return false
                    if (jump.message && !isEqual(jump.message, row.message))
                        return false
                    jump.sourceSnapshot = currentSnapshot
                    jump.message = cloneDeep(row.message)
                }
                const startOffset = hasConversationStart() ? 1 : 0
                const key = currentMessageKey(index)
                if (key === undefined) return false
                const result = reconcileViewport({
                    jumpTarget: index + startOffset,
                    preserveAnchor: false,
                })
                if (!result?.jumpAccepted) return false
                const rowMounted = await waitForRowMount(key, generation)
                if (currentPendingJump() === jump && generation !== jump.generation)
                    continue
                if (!rowMounted || currentPendingJump() !== jump || generation !== navigationGeneration || scope !== currentChatScope()) return false
                await waitForLayout()
                if (currentPendingJump() === jump && generation !== jump.generation)
                    continue
                if (
                    generation !== navigationGeneration ||
                    scope !== currentChatScope()
                )
                    return false
                if (!isPendingJumpSourceCurrent(jump)) return false
                if (
                    jump.message &&
                    !isEqual(
                        currentSourceSnapshot()?.rowAt(index)?.message,
                        jump.message,
                    )
                )
                    return false
                const element = mountedElements.get(key)
                if (!element) return false
                suppressScroll = true
                scrollRowIntoView(element, options)
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
                    relativeOffset:
                        element.getBoundingClientRect().top -
                        (scrollContainer?.getBoundingClientRect().top ?? 0),
                }
                suppressScroll = false
                if (scrollContainer) lastScrollTop = scrollContainer.scrollTop
                return true
            }
            return false
        } finally {
            if (pendingJump === jump) pendingJump = null
        }
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

    function scrollRowIntoView(
        element: HTMLElement,
        options: ChatViewportJumpOptions,
    ): void {
        if (options.align === 'end' && typeof scrollContainer?.scrollBy === 'function') {
            const visibleBottom =
                scrollContainer.getBoundingClientRect().bottom - (options.bottomInset ?? 0)
            scrollContainer.scrollBy({
                top: element.getBoundingClientRect().bottom - visibleBottom,
                behavior: 'instant',
            })
            return
        }
        element.scrollIntoView?.({ behavior: 'instant', block: options.align ?? 'start' })
    }

    async function jumpToConversationStartRow(
        options: ChatViewportJumpOptions = {},
    ): Promise<boolean> {
        if (!hasConversationStart()) return currentMessageCount() > 0 && jumpTo(0, options)
        // Loads and mounts the oldest window; the start row sits right above message 0.
        if (currentMessageCount() > 0 && !(await jumpTo(0))) return false
        const scope = currentChatScope()
        const key = conversationStartKey(scope)
        if (!mountedElements.has(key)) {
            reconcileViewport({ jumpTarget: 0, preserveAnchor: false })
            await waitForLayout()
            if (destroyed || scope !== currentChatScope()) return false
        }
        const element = mountedElements.get(key)
        if (!element) return false
        initialLatestFollow = false
        suppressScroll = true
        scrollRowIntoView(element, options)
        viewportAnchor = {
            key,
            indexHint: 0,
            relativeOffset:
                element.getBoundingClientRect().top -
                (scrollContainer?.getBoundingClientRect().top ?? 0),
        }
        suppressScroll = false
        if (scrollContainer) lastScrollTop = scrollContainer.scrollTop
        return true
    }

    export function jumpToTop(): Promise<boolean> {
        return jumpToConversationStartRow()
    }

    export async function jumpToBottom(): Promise<boolean> {
        hasNewUnreadMessage = false
        const totalMessages = currentMessageCount()
        const placed = totalMessages > 0
            ? await jumpTo(totalMessages - 1)
            : await jumpToConversationStartRow()
        if (!placed || !scrollContainer) return placed
        suppressScroll = true
        // The chat scrolls in reverse, so zero is the latest position.
        scrollContainer.scrollTop = 0
        viewportAnchor = captureDomAnchor()
        suppressScroll = false
        lastScrollTop = scrollContainer.scrollTop
        return true
    }

    export function navigateMessage(
        direction: ChatViewportStepDirection,
        bottomInset = 0,
    ): Promise<boolean> {
        if (!scrollContainer) return Promise.resolve(false)
        const containerRect = scrollContainer.getBoundingClientRect()
        const rows: ChatViewportRowBounds[] = []
        for (const element of mountedElements.values()) {
            const index = Number(element.dataset.chatViewportIndex)
            if (!element.isConnected || !Number.isInteger(index)) continue
            const rect = element.getBoundingClientRect()
            rows.push({ index, top: rect.top, bottom: rect.bottom })
        }
        const startOffset = hasConversationStart() ? 1 : 0
        const target = resolveChatViewportStep(
            rows,
            containerRect.top,
            containerRect.bottom - bottomInset,
            currentMessageCount() + startOffset,
            direction,
        )
        if (target.kind === 'top') return jumpToTop()
        if (target.kind === 'bottom') return jumpToBottom()
        const options = { align: target.align, bottomInset }
        return target.index < startOffset
            ? jumpToConversationStartRow(options)
            : jumpTo(target.index - startOffset, options)
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
            for (const element of mountedElements.values()) resizeObserver.observe(element, { box: 'border-box' })
            if (scrollContainer) resizeObserver.observe(scrollContainer, { box: 'border-box' })
        }
        reconcileViewport()
        chatBody.addEventListener('focusin', handleFocusIn)
        chatBody.addEventListener('focusout', handleFocusOut)
        chatBody.addEventListener('play', handleMediaPlay, true)
        chatBody.addEventListener('pause', handleMediaStop, true)
        chatBody.addEventListener('ended', handleMediaStop, true)
        scrollContainer?.addEventListener('scroll', handleScroll)
        scrollContainer?.addEventListener('wheel', handleUserScrollIntent, {
            passive: true,
        })
        scrollContainer?.addEventListener('touchmove', handleUserScrollIntent, {
            passive: true,
        })
        scrollContainer?.addEventListener('pointerdown', handleUserScrollIntent)
        scrollContainer?.addEventListener('keydown', handleUserScrollIntent)
        window.addEventListener('pointerup', releaseScrollPointer)
        window.addEventListener('pointercancel', releaseScrollPointer)
        const unsubscribeProfile = subscribeRuntimePerformanceProfile(() => reconcileViewport())
        return () => {
            unsubscribeProfile()
            chatBody.removeEventListener('focusin', handleFocusIn)
            chatBody.removeEventListener('focusout', handleFocusOut)
            chatBody.removeEventListener('play', handleMediaPlay, true)
            chatBody.removeEventListener('pause', handleMediaStop, true)
            chatBody.removeEventListener('ended', handleMediaStop, true)
            scrollContainer?.removeEventListener('scroll', handleScroll)
            scrollContainer?.removeEventListener('wheel', handleUserScrollIntent)
            scrollContainer?.removeEventListener('touchmove', handleUserScrollIntent)
            scrollContainer?.removeEventListener('pointerdown', handleUserScrollIntent)
            scrollContainer?.removeEventListener('keydown', handleUserScrollIntent)
            window.removeEventListener('pointerup', releaseScrollPointer)
            window.removeEventListener('pointercancel', releaseScrollPointer)
            resizeObserver?.disconnect()
            resizeObserver = null
            retainScreenAnchor()
            scrollContainer = null
        }
    })

    onDestroy(() => {
        retainScreenAnchor()
        destroyed = true
        latestPairController.abort()
        navigationGeneration += 1
        cancelStaleRowMountWaiters()
        abortSourceLoads()
        releaseSourcePins()
        sourceUnsubscribe?.()
        sourceUnsubscribe = null
        activeViewportSource = null
        clearScheduledWork()
        clearMountedRows()
        chatView.dispose()
        renderSignatures.clear()
        measuredHeights.clear()
        measuredHeightIndices.clear()
        measuredHeightIndexByKey.clear()
        measuredHeightRecency.clear()
        pinReasons.clear()
        blurredEditorPins.clear()
        playingMedia.clear()
        sourceHandoffRuntimeKeys.clear()
        imageResolutionGeneration += 1
    })

</script>

{#if initialRowsLoading || initialRowsLoadFailed}
    <div
        class="pointer-events-none absolute inset-0 z-10 flex items-center justify-center bg-bgcolor"
        data-chat-initial-loading
    >
        {#if initialRowsLoadFailed}
            <div
                class="pointer-events-auto flex flex-col items-center gap-3 text-center"
                data-chat-load-error
                role="alert"
            >
                <span>{language.chatDataLoadFailed}</span>
                <button
                    class="rounded-lg border border-borderc px-3 py-1.5 hover:bg-darkbg"
                    data-chat-load-retry
                    onclick={retryInitialSourceRows}
                >
                    {language.hypaV3Modal.retry}
                </button>
            </div>
        {:else}
            <LoadingIndicator label={language.loadingChatData} />
        {/if}
    </div>
{:else if parserProjectionLoadFailed || historyRowsLoadFailed || latestPairLoadFailed}
    <div
        class="absolute bottom-2 left-2 right-2 z-10 flex items-center justify-center gap-3 rounded-lg border border-borderc bg-bgcolor p-3 text-center"
        data-chat-load-error
        role="alert"
    >
        <span>{language.chatDataLoadFailed}</span>
        <button
            class="rounded-lg border border-borderc px-3 py-1.5 hover:bg-darkbg"
            data-chat-load-retry
            onclick={retryInitialSourceRows}
        >
            {language.hypaV3Modal.retry}
        </button>
    </div>
{/if}
<div
    class="flex flex-col-reverse"
    bind:this={chatBody}
></div>
