import type { Chat } from '../storage/database.svelte'
import { getGenerationOutputPhase, type GenerationOutputPhase } from '../process/generationOutputPhase'
import { prepareGenerationMetadataPatch } from '../process/generationRequestPhase'
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
    commitStreamingPatch(request: ConversationPatchRequest, signal?: AbortSignal, intervalOwner?: GenerationOutputPhase): Promise<ConversationPatchResult | null>
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
        duringRequest = false,
    ): Promise<ConversationPatchResult> {
        const attempt: { outcome?: ConversationPatchResult; adopt?: (revision: DataRevision) => Promise<void> } = {}
        const revision = await dependencies.commitPreparedUnitIntent(PATCH_REASON, async (reader) => {
            // Runs after pending writes are flushed, with no other write in between until the commit lands.
            if (!admitted()) {
                attempt.outcome = busy()
                return null
            }
            throwIfAborted(signal)
            const planned = await planStoredConversationPatch(reader, request)
            throwIfAborted(signal)
            if (!admitted()) {
                attempt.outcome = busy()
                return null
            }
            if (planned.kind === 'conflict') {
                attempt.outcome = { status: 'conflict', conflict: planned.conflict, revision: reader.revision }
                return null
            }
            if (duringRequest) attempt.adopt = prepareGenerationMetadataPatch(request, reader.revision) ?? undefined
            return prepareConversationPatchCommit(request, planned)
        })
        if (revision !== null) await attempt.adopt?.(revision)
        return attempt.outcome ?? { status: 'applied', revision: revision! }
    }

    const commitToOtherConversation = async (request: ConversationPatchRequest, signal: AbortSignal | undefined) => {
        const result = await commitThroughStore(request, signal, () => !dependencies.isConversationGenerating(request))
        const phase = getGenerationOutputPhase(request)
        return result.status === 'busy' && (phase?.phase === 'streaming' || phase?.pluginWritesOpen)
            ? patchConversation(request, signal) : result
    }

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
        }, true)
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
            if (dependencies.isConversationGenerating(request)) return patchConversation(request, signal)
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

    async function patchConversation(request: ConversationPatchRequest, signal?: AbortSignal): Promise<ConversationPatchResult> {
        const output = getGenerationOutputPhase(request)
        if (output?.phase === 'processing') {
            if (!output.pluginWritesOpen) return busy()
            const write = output.holdWrite()
            try {
                return await dependencies.commitStreamingPatch(request, signal, output) ?? busy()
            } finally {
                write.release()
            }
        }
        if (output?.phase === 'streaming') {
            const result = await dependencies.commitStreamingPatch(request, signal)
            // The generation ended before the queued write could acquire the owner.
            if (result) return result
            return patchConversation(request, signal)
        }
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
    }
    return { patchConversation }
}
