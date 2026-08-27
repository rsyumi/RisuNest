import { ChatRenderIdentityRegistry } from './chatRenderIdentity'
import type {
    CapturedChatMessageTarget,
    CurrentChatMessageTarget,
} from './chatMessageUi'
import {
    type ActiveConversationMutationEvent,
    type ActiveConversationPin,
    type ActiveConversationSession,
} from './storage/activeConversationSession'
import type { Message } from './storage/database.svelte'

declare const conversationViewportKeyBrand: unique symbol

export type ConversationViewportKey = string & {
    readonly [conversationViewportKeyBrand]: true
}

export interface ConversationViewportRow {
    readonly key: ConversationViewportKey
    readonly absoluteIndex: number
    readonly message: Readonly<Message>
    readonly sourceVersion: number
}

export interface ConversationViewportSnapshot {
    readonly sourceToken: string
    readonly version: number
    readonly totalMessages: number
    keyAt(absoluteIndex: number): ConversationViewportKey | undefined
    indexOfKey(key: ConversationViewportKey): number
    rowAt(absoluteIndex: number): ConversationViewportRow | undefined
}

export type ConversationViewportLoadReason = 'viewport' | 'jump' | 'streaming'
export type ConversationViewportPinReason =
    | 'viewport'
    | 'editor'
    | 'playing-media'
    | 'streaming'

export interface ConversationViewportRangeRequest {
    startIndex: number
    limit: number
    reason: ConversationViewportLoadReason
    signal?: AbortSignal
}

export interface ConversationViewportPin {
    release(): void
}

export interface ConversationViewportSource {
    snapshot(): ConversationViewportSnapshot
    ensureRange(input: ConversationViewportRangeRequest): Promise<void>
    acquireRangePin(
        startIndex: number,
        endIndex: number,
        reason: ConversationViewportPinReason,
    ): ConversationViewportPin
    subscribe(listener: () => void): () => void
    captureMessageTarget(key: ConversationViewportKey): CapturedChatMessageTarget | null
    dispose(): void
}

export interface SynchronousSessionConversationViewportSourceOptions {
    session: ActiveConversationSession
    captureCurrent(): CurrentChatMessageTarget | null
}

let nextSourceToken = 0

function createSourceToken(): string {
    nextSourceToken += 1
    return `conversation-viewport-source-${nextSourceToken}`
}

export class SynchronousSessionConversationViewportSource
implements ConversationViewportSource {
    readonly sourceToken = createSourceToken()

    private readonly session: ActiveConversationSession
    private readonly captureCurrent: () => CurrentChatMessageTarget | null
    private readonly identityRegistry = new ChatRenderIdentityRegistry()
    private readonly listeners = new Set<() => void>()
    private readonly pins = new Set<ActiveConversationPin>()
    private readonly unsubscribeSession: () => void
    private keys: ConversationViewportKey[]
    private keyIndices = new Map<ConversationViewportKey, number>()
    private rows = new Map<number, ConversationViewportRow>()
    private currentVersion: number
    private nextInsertedKey = 0
    private disposed = false

    constructor(options: SynchronousSessionConversationViewportSourceOptions) {
        this.session = options.session
        this.captureCurrent = options.captureCurrent
        this.currentVersion = this.session.version
        this.keys = this.createInitialKeys()
        this.rebuildKeyIndices()
        this.unsubscribeSession = this.session.subscribe((event) => {
            this.handleSessionChange(event)
        })
    }

    snapshot(): ConversationViewportSnapshot {
        const keys = this.keys
        const keyIndices = this.keyIndices
        const rows = this.rows
        return {
            sourceToken: this.sourceToken,
            version: this.currentVersion,
            totalMessages: keys.length,
            keyAt: (absoluteIndex) => keys[absoluteIndex],
            indexOfKey: (key) => keyIndices.get(key) ?? -1,
            rowAt: (absoluteIndex) => rows.get(absoluteIndex),
        }
    }

    async ensureRange(input: ConversationViewportRangeRequest): Promise<void> {
        this.assertUsable()
        if (input.signal?.aborted) return
        const sourceVersion = this.currentVersion
        const window = this.session.readRange(input.startIndex, input.limit)
        void input.reason

        await Promise.resolve()
        if (
            input.signal?.aborted ||
            this.disposed ||
            sourceVersion !== this.currentVersion ||
            window.sessionVersion !== this.currentVersion ||
            !this.session.isActive
        ) return

        const nextRows = new Map(this.rows)
        for (let offset = 0; offset < window.messages.length; offset++) {
            const absoluteIndex = window.startIndex + offset
            const key = this.keys[absoluteIndex]
            if (key === undefined) continue
            nextRows.set(absoluteIndex, {
                key,
                absoluteIndex,
                message: window.messages[offset],
                sourceVersion,
            })
        }
        this.rows = nextRows
        this.notifyListeners()
    }

    acquireRangePin(
        startIndex: number,
        endIndex: number,
        reason: ConversationViewportPinReason,
    ): ConversationViewportPin {
        this.assertUsable()
        const sessionPin = this.session.acquireRangePin(startIndex, endIndex, reason)
        this.pins.add(sessionPin)
        let released = false
        return {
            release: () => {
                if (released) return
                released = true
                this.pins.delete(sessionPin)
                sessionPin.release()
            },
        }
    }

    subscribe(listener: () => void): () => void {
        this.assertUsable()
        this.listeners.add(listener)
        let subscribed = true
        return () => {
            if (!subscribed) return
            subscribed = false
            this.listeners.delete(listener)
        }
    }

    captureMessageTarget(key: ConversationViewportKey): CapturedChatMessageTarget | null {
        if (this.disposed || !this.session.isActive) return null
        const absoluteIndex = this.keyIndices.get(key)
        if (absoluteIndex === undefined) return null
        const version = this.currentVersion
        const current = this.captureCurrent()
        if (!current || !this.session.matchesConversation(current.character.chaId, current.conversation)) {
            return null
        }
        try {
            const locator = this.session.locate(absoluteIndex)
            const message = this.session.readMessage(locator)
            if (
                version !== this.currentVersion ||
                this.keys[absoluteIndex] !== key ||
                !this.session.isActive
            ) return null
            return {
                kind: 'session',
                absoluteIndex,
                ...current,
                message,
                session: this.session,
                locator,
            }
        } catch {
            return null
        }
    }

    dispose(): void {
        if (this.disposed) return
        this.disposed = true
        this.unsubscribeSession()
        for (const pin of [...this.pins]) pin.release()
        this.pins.clear()
        this.keys = []
        this.keyIndices = new Map()
        this.rows = new Map()
        this.notifyListeners()
        this.listeners.clear()
    }

    private handleSessionChange(event: ActiveConversationMutationEvent | null): void {
        if (!event || !this.session.isActive) {
            this.dispose()
            return
        }
        this.reconcileKeys(event)
        this.currentVersion = this.session.version
        this.rows = new Map()
        this.rebuildKeyIndices()
        this.notifyListeners()
    }

    private createInitialKeys(): ConversationViewportKey[] {
        return this.identityRegistry
            .register(this.sourceToken, this.session.materializeCompatibilityArray())
            .toArray() as ConversationViewportKey[]
    }

    private reconcileKeys(event: ActiveConversationMutationEvent): void {
        if (event.previousVersion !== this.currentVersion) {
            this.keys = this.createInitialKeys()
            return
        }

        const nextKeys = [...this.keys]
        for (const mutation of event.mutations) {
            if (
                mutation.start > nextKeys.length ||
                mutation.deleteCount > nextKeys.length - mutation.start
            ) {
                this.keys = this.createInitialKeys()
                return
            }
            const retainedCount = Math.min(mutation.deleteCount, mutation.messages.length)
            const replacements = nextKeys.slice(
                mutation.start,
                mutation.start + retainedCount,
            )
            while (replacements.length < mutation.messages.length) {
                replacements.push(this.createInsertedKey())
            }
            nextKeys.splice(mutation.start, mutation.deleteCount, ...replacements)
        }
        this.keys = nextKeys.length === this.session.totalMessages
            ? nextKeys
            : this.createInitialKeys()
    }

    private rebuildKeyIndices(): void {
        this.keyIndices = new Map(this.keys.map((key, index) => [key, index]))
    }

    private createInsertedKey(): ConversationViewportKey {
        this.nextInsertedKey += 1
        return `${this.sourceToken}|inserted:${this.nextInsertedKey}` as ConversationViewportKey
    }

    private notifyListeners(): void {
        for (const listener of [...this.listeners]) {
            try {
                listener()
            } catch (error) {
                console.error('Conversation viewport subscriber failed', error)
            }
        }
    }

    private assertUsable(): void {
        if (this.disposed || !this.session.isActive) {
            throw new Error('Conversation viewport source is disposed')
        }
    }
}
