import type { Chat, Message } from './database.svelte'
import {
    ActiveConversationSession,
    ConversationSessionInactiveError,
    ConversationSessionStaleError,
    type ActiveConversationBackwardScan,
    type ActiveConversationPin,
    type ActiveConversationWindow,
} from './activeConversationSession'
import type { DataRevision } from './persistentDataStore'
import { safeStructuredClone } from '../polyfill'

export type ConversationHistoryOperationSource = 'active-session' | 'compatibility-snapshot'

export interface ConversationHistoryOperation {
    readonly source: ConversationHistoryOperationSource
    readonly characterId: string
    readonly conversationId: string
    readonly storeRevision: DataRevision
    readonly sessionVersion: number
    readonly totalMessages: number
    readLatest(limit: number): ActiveConversationWindow
    readRange(startIndex: number, limit: number): ActiveConversationWindow
    scanBackward(startIndexExclusive?: number, limit?: number): ActiveConversationBackwardScan
    assertCurrent(): void
    dispose(): void
}

export class ConversationHistoryOperationDisposedError extends Error {
    constructor() {
        super('Conversation history operation is disposed')
        this.name = 'ConversationHistoryOperationDisposedError'
    }
}

class SessionConversationHistoryOperation implements ConversationHistoryOperation {
    readonly characterId: string
    readonly conversationId: string
    readonly storeRevision: DataRevision
    readonly sessionVersion: number
    readonly totalMessages: number

    private session: ActiveConversationSession | null
    private pin: ActiveConversationPin | null

    constructor(
        session: ActiveConversationSession,
        readonly source: ConversationHistoryOperationSource,
        private readonly ownsSession: boolean,
    ) {
        this.session = session
        this.characterId = session.characterId
        this.conversationId = session.conversationId
        this.storeRevision = session.storeRevision
        this.sessionVersion = session.version
        this.totalMessages = session.totalMessages
        this.pin = session.acquirePin('prompt')
    }

    readLatest(limit: number): ActiveConversationWindow {
        const session = this.requireCurrentSession()
        const result = session.readLatest(limit)
        this.assertResultVersion(result.sessionVersion)
        return result
    }

    readRange(startIndex: number, limit: number): ActiveConversationWindow {
        const session = this.requireCurrentSession()
        const result = session.readRange(startIndex, limit)
        this.assertResultVersion(result.sessionVersion)
        return result
    }

    scanBackward(
        startIndexExclusive = this.totalMessages,
        limit?: number,
    ): ActiveConversationBackwardScan {
        const session = this.requireCurrentSession()
        const result = limit === undefined
            ? session.scanBackward(startIndexExclusive)
            : session.scanBackward(startIndexExclusive, limit)
        this.assertResultVersion(result.sessionVersion)
        return result
    }

    assertCurrent(): void {
        this.requireCurrentSession()
    }

    dispose(): void {
        const session = this.session
        if (!session) return
        this.pin?.release()
        this.pin = null
        if (this.ownsSession) session.invalidate()
        this.session = null
    }

    private requireCurrentSession(): ActiveConversationSession {
        const session = this.session
        if (!session) throw new ConversationHistoryOperationDisposedError()
        if (!session.isActive) throw new ConversationSessionInactiveError()
        if (session.version !== this.sessionVersion) {
            throw new ConversationSessionStaleError(this.sessionVersion, session.version)
        }
        return session
    }

    private assertResultVersion(version: number): void {
        if (version !== this.sessionVersion) {
            throw new ConversationSessionStaleError(this.sessionVersion, version)
        }
    }
}

export function beginPinnedConversationHistoryOperation(
    session: ActiveConversationSession,
): ConversationHistoryOperation {
    return new SessionConversationHistoryOperation(session, 'active-session', false)
}

export function createCompatibilityConversationHistorySnapshot(options: {
    characterId: string
    conversationId: string
    messages: readonly Message[]
    storeRevision: DataRevision
}): ConversationHistoryOperation {
    const conversation: Chat = {
        id: options.conversationId,
        name: '',
        note: '',
        localLore: [],
        message: safeStructuredClone([...options.messages]),
    }
    const session = new ActiveConversationSession({
        characterId: options.characterId,
        conversationId: options.conversationId,
        conversation,
        storeRevision: options.storeRevision,
    })
    return new SessionConversationHistoryOperation(session, 'compatibility-snapshot', true)
}
