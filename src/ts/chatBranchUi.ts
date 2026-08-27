import {
    resolveChatMessageTarget,
    type CapturedChatMessageTarget,
    type ChatMessageUiContext,
} from './chatMessageUi'
import { safeStructuredClone } from './polyfill'
import type { Chat, Message } from './storage/database.svelte'
import type {
    PersistentDataRuntime,
    PersistentDestructiveReplacementFence,
} from './storage/persistentDataRuntime'
import type { CharacterDetail } from './storage/persistentDataStore'
import {
    copyPinnedConversationBranch,
    type PinnedConversationBranchResult,
} from './storage/conversationBranchJobs'

class CapturedConversationBranchStaleError extends Error {
    constructor() {
        super('The retained conversation branch target is stale')
        this.name = 'CapturedConversationBranchStaleError'
    }
}

export interface CreateCapturedConversationBranchOptions {
    target: CapturedChatMessageTarget
    context: ChatMessageUiContext
    runtime: Pick<
        PersistentDataRuntime,
        'store' | 'capturePersistentMutationToken' | 'acquireDestructiveReplacementFence'
    >
    createFolderOnBranch: boolean
    createId(): string
    createBranchName(sourceName: string): string
    navigateToBranch(id: string): Promise<boolean>
    pageSize?: number
    signal?: AbortSignal
}

function conversationDetail(conversation: Chat): Omit<Chat, 'message'> {
    const { message: _messages, ...detail } = conversation
    return safeStructuredClone(detail)
}

function characterDetail(
    character: CapturedChatMessageTarget['character'],
): CharacterDetail {
    const { chats: _chats, ...detail } = character
    return safeStructuredClone(detail) as CharacterDetail
}

function branchMarker(source: Chat, message: Message, markerId: string): Message {
    return {
        role: 'char',
        data: '{{specialcomment::branchedfrom::' + source.id + '::' + source.name + '::'
            + message.chatId + '::}}',
        isComment: true,
        disabled: true,
        chatId: markerId,
    }
}

function requireCurrentTarget(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
): CapturedChatMessageTarget {
    const current = resolveChatMessageTarget(target, context)
    if (!current?.session || !current.locator) {
        throw new CapturedConversationBranchStaleError()
    }
    return current
}

export async function createCapturedConversationBranch(
    options: CreateCapturedConversationBranchOptions,
): Promise<boolean> {
    try {
        requireCurrentTarget(options.target, options.context)
    } catch (error) {
        if (error instanceof CapturedConversationBranchStaleError) return false
        throw error
    }

    options.signal?.throwIfAborted()
    const mutationToken = await options.runtime.capturePersistentMutationToken(
        'create-conversation-branch',
    )
    let current: CapturedChatMessageTarget
    try {
        current = requireCurrentTarget(options.target, options.context)
    } catch (error) {
        if (error instanceof CapturedConversationBranchStaleError) return false
        throw error
    }

    const source = current.conversation
    const sourceMessage = current.message
    if (!sourceMessage) return false

    const nextCharacter = characterDetail(current.character)
    nextCharacter.chatPage = 0
    let nextSource: Omit<Chat, 'message'> | undefined
    let folderId = source.folderId
    if (options.createFolderOnBranch && !folderId) {
        folderId = options.createId()
        nextCharacter.chatFolders = [
            {
                id: folderId,
                name: `Branches of ${source.name}`,
                folded: false,
            },
            ...(nextCharacter.chatFolders ?? []),
        ]
        nextSource = {
            ...conversationDetail(source),
            folderId,
        }
    }

    const branchId = options.createId()
    const branch = {
        ...conversationDetail(source),
        name: options.createBranchName(source.name),
        id: branchId,
        ...(folderId === undefined ? {} : { folderId }),
    } satisfies Omit<Chat, 'message'>
    const marker = branchMarker(source, sourceMessage, options.createId())

    let fence: PersistentDestructiveReplacementFence | null = null
    let result: PinnedConversationBranchResult
    try {
        result = await copyPinnedConversationBranch(options.runtime.store, {
            characterId: current.character.chaId,
            sourceConversationId: source.id!,
            sourceRevision: mutationToken.revision,
            inclusiveEndIndex: current.absoluteIndex,
            branch,
            branchMarker: marker,
            character: nextCharacter,
            sourceConversation: nextSource,
            pageSize: options.pageSize,
            signal: options.signal,
            async beforeCommit() {
                requireCurrentTarget(options.target, options.context)
                fence = await options.runtime.acquireDestructiveReplacementFence(mutationToken)
                try {
                    requireCurrentTarget(options.target, options.context)
                } catch (error) {
                    fence.release()
                    fence = null
                    throw error
                }
            },
        })
        if (!fence) throw new Error('Conversation branch commit did not acquire a mutation fence')
        await fence.refreshCommittedWorkingSet(result.branchRevision, {
            forceScalableProjection: false,
        })
    } catch (error) {
        if (error instanceof CapturedConversationBranchStaleError) return false
        throw error
    } finally {
        fence?.release()
    }

    return options.navigateToBranch(result.branchConversationId)
}
