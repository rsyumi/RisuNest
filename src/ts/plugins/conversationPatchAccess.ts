import type { Chat } from '../storage/database.svelte'
import type { ActiveConversationSession } from '../storage/activeConversationSession'
import type { CompleteConversationLease, SelectedConversationTarget } from '../storage/activeWorkingSet.svelte'
import type { DataRevision, GeneratingConversation, PersistentRevisionReader } from '../storage/persistentDataStore'
import type { PreparedUnitIntent, WindowedConversationPersistenceAuthority } from '../storage/saveCoordinator'
import { cloneConversationMetadata } from '../storage/selectedConversationLifecycle'
import {
    patchSetsMessageData,
    planLiveConversationPatch,
    planStoredConversationPatch,
    prepareConversationPatchCommit,
    type ConversationPatchRequest,
    type ConversationPatchResult,
} from './conversationPatch'
import { throwIfAborted } from './pluginQueryInput'

const PATCH_REASON = 'plugin-conversation-patch'

export interface ConversationPatchAccessDependencies {
    flushPendingData(reason: string): Promise<void>
    getPersistentRevision(): DataRevision
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    acquireCompleteConversation(reason: string, target: SelectedConversationTarget): Promise<CompleteConversationLease>
    getActiveConversationSession(): ActiveConversationSession | null
    captureSelectedConversationAuthority?(): WindowedConversationPersistenceAuthority | null
    /** The live object of the selected conversation, the one its session renders. */
    getSelectedConversation(): Chat | null
    /** The selected conversation while a generation runs, or a conversation being rerolled. */
    isConversationGenerating(target: GeneratingConversation): boolean
    isGenerationRequestPhaseOpen(target: GeneratingConversation): boolean
    trackConversationPatch(target: GeneratingConversation): () => void
    commitPreparedUnitIntent(
        reason: string,
        prepare: (reader: PersistentRevisionReader) => Promise<PreparedUnitIntent | null>,
    ): Promise<DataRevision | null>
}

export interface ConversationPatchAccess {
    patchConversation(request: ConversationPatchRequest, signal?: AbortSignal): Promise<ConversationPatchResult>
}

export function createConversationPatchAccess(dependencies: ConversationPatchAccessDependencies): ConversationPatchAccess {
    const busy = (): ConversationPatchResult => ({ status: 'busy', revision: dependencies.getPersistentRevision() })
    const selectedTarget = (request: ConversationPatchRequest) => {
        const selected = dependencies.captureSelectedConversationTarget()
        return selected?.characterId === request.characterId && selected.conversationId === request.conversationId
            ? selected
            : null
    }

    async function commitThroughStore(
        request: ConversationPatchRequest,
        signal: AbortSignal | undefined,
        admitted: () => boolean,
    ): Promise<ConversationPatchResult> {
        let outcome = null as ConversationPatchResult | null
        const revision = await dependencies.commitPreparedUnitIntent(PATCH_REASON, async (reader) => {
            // Runs after pending writes are flushed, with no other write in between until the commit lands.
            if (!admitted()) {
                outcome = busy()
                return null
            }
            throwIfAborted(signal)
            const planned = await planStoredConversationPatch(reader, request)
            throwIfAborted(signal)
            if (!admitted()) {
                outcome = busy()
                return null
            }
            if (planned.kind === 'conflict') {
                outcome = { status: 'conflict', conflict: planned.conflict, revision: reader.revision }
                return null
            }
            return prepareConversationPatchCommit(request, planned)
        })
        return outcome ?? { status: 'applied', revision: revision! }
    }

    const commitToOtherConversation = (request: ConversationPatchRequest, signal: AbortSignal | undefined) =>
        commitThroughStore(request, signal, () => !dependencies.isConversationGenerating(request))

    // The session adopts the commit as persisted metadata, which keeps the generation's
    // continuation point; the generation waits for this patch before it applies its response.
    const commitDuringRequest = (request: ConversationPatchRequest, signal: AbortSignal | undefined) => {
        const initialTarget = selectedTarget(request)
        const initialSession = dependencies.getActiveConversationSession()
        const initialAuthority = dependencies.captureSelectedConversationAuthority?.()
        return commitThroughStore(request, signal, () => {
            const selected = selectedTarget(request)
            if (!selected || !initialTarget || selected.navigationGeneration !== initialTarget.navigationGeneration) return false
            const session = dependencies.getActiveConversationSession()
            const conversation = dependencies.getSelectedConversation()
            if (initialSession) return session === initialSession && conversation !== null &&
                session.matchesConversation(request.characterId, conversation) && session.canAdoptPersistedMetadata
            const authority = dependencies.captureSelectedConversationAuthority?.()
            return !!initialAuthority && !!authority &&
                authority.sessionToken === initialAuthority.sessionToken &&
                authority.characterId === request.characterId && authority.conversationId === request.conversationId &&
                authority.sessionVersion === authority.persistedSessionVersion
        })
    }

    async function commitToSelectedConversation(
        request: ConversationPatchRequest,
        signal: AbortSignal | undefined,
        selected: SelectedConversationTarget,
    ): Promise<ConversationPatchResult> {
        let lease: CompleteConversationLease
        try {
            lease = await dependencies.acquireCompleteConversation(PATCH_REASON, selected)
        } catch (error) {
            if (selectedTarget(request)) throw error
            return commitToOtherConversation(request, signal)
        }
        try {
            await dependencies.flushPendingData(PATCH_REASON)
            throwIfAborted(signal)
            if (dependencies.isConversationGenerating(request)) return busy()
            const session = lease.session
            const conversation = dependencies.getSelectedConversation()
            if (!selectedTarget(request) || !conversation || !session.matchesConversation(request.characterId, conversation)) {
                return commitToOtherConversation(request, signal)
            }
            const revision = dependencies.getPersistentRevision()
            const messages = session.materializeCompatibilityArray()
            const planned = planLiveConversationPatch(request, messages, conversation.scriptstate as Record<string, unknown> | undefined)
            if (planned.kind === 'conflict') return { status: 'conflict', conflict: planned.conflict, revision }
            if (!planned.runs.length && !planned.scriptstate) return { status: 'applied', revision }
            const expectedMetadata = cloneConversationMetadata(conversation)
            session.applyOperation({
                expectedVersion: session.version,
                expectedMetadata,
                metadata: planned.scriptstate ? { ...expectedMetadata, scriptstate: planned.scriptstate } : expectedMetadata,
                ranges: planned.runs.map((run) => ({
                    position: session.positionAt(run.start),
                    deleteCount: run.messages.length,
                    messages: run.messages,
                })),
            })
            await dependencies.flushPendingData(PATCH_REASON)
            return { status: 'applied', revision: dependencies.getPersistentRevision() }
        } finally {
            lease.release()
        }
    }

    return {
        async patchConversation(request, signal) {
            if (dependencies.isConversationGenerating(request)) {
                if (patchSetsMessageData(request) || !dependencies.isGenerationRequestPhaseOpen(request)) return busy()
                const release = dependencies.trackConversationPatch(request)
                try {
                    return await commitDuringRequest(request, signal)
                } finally {
                    release()
                }
            }
            const selected = selectedTarget(request)
            return selected
                ? commitToSelectedConversation(request, signal, selected)
                : commitToOtherConversation(request, signal)
        },
    }
}
