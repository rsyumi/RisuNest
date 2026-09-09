import { afterEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))

import { ReloadGUIPointer, selectedCharID } from '../stores.svelte'
import {
    ActiveWorkingSet,
    type WorkingSetCoordinator,
} from './activeWorkingSet.svelte'
import {
    getDatabase,
    setDatabaseLite,
    type Chat,
    type Database,
} from './database.svelte'
import { createProductionStateAdapter } from './persistentDataRuntime.svelte'
import type { PersistentDataStore } from './persistentDataStore'
import { workingSetResidency } from './workingSetResidency'

afterEach(() => {
    workingSetResidency.clear()
    selectedCharID.set(-1)
})

describe('selected conversation production publication identity', () => {
    it('still refreshes the global UI when selecting a different conversation', () => {
        const first = {
            id: 'synthetic-first',
            message: [{ role: 'user', data: 'Synthetic first message' }],
        } as Chat
        const second = {
            id: 'synthetic-second',
            message: [{ role: 'char', data: 'Synthetic second message' }],
        } as Chat
        setDatabaseLite({
            characters: [{
                type: 'character',
                chaId: 'synthetic-character',
                chatPage: 0,
                chats: [first, second],
            }],
            botPresets: [],
            plugins: [],
        } as unknown as Database)
        selectedCharID.set(0)
        const reload = vi.spyOn(ReloadGUIPointer, 'set')
        try {
            createProductionStateAdapter().publishConversation(
                'synthetic-character',
                second,
            )
            expect(getDatabase().characters[0].chatPage).toBe(1)
            expect(reload).toHaveBeenCalledOnce()
        } finally {
            reload.mockRestore()
        }
    })

    it('promotes a directly activated metadata shell through the live Svelte database', async () => {
        const conversation: Chat = {
            id: 'synthetic-conversation',
            name: 'Synthetic conversation',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'Synthetic message' }],
        }
        const detail = {
            type: 'character' as const,
            chaId: 'synthetic-character',
            name: 'Synthetic character',
            chatPage: 0,
        }
        const { message, ...metadata } = conversation
        setDatabaseLite({
            characters: [{ ...detail, chats: [conversation] }],
            botPresets: [],
            plugins: [],
        } as unknown as Database)
        selectedCharID.set(-1)
        workingSetResidency.clear()
        const adapter = createProductionStateAdapter()
        const readConversation = vi.fn(async () => ({
            revision: 1,
            value: structuredClone(conversation),
        }))
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 1, value: detail })),
            queryConversations: vi.fn(async () => ({
                revision: 1,
                items: [
                    {
                        id: conversation.id,
                        characterId: detail.chaId,
                        name: conversation.name,
                        configuredIndex: 0,
                        recentAt: 0,
                        messageCount: message.length,
                    },
                ],
            })),
            readConversationMetadata: vi.fn(async () => ({
                revision: 1,
                value: {
                    characterId: detail.chaId,
                    conversationId: conversation.id,
                    conversation: metadata,
                    totalMessages: message.length,
                },
            })),
            readConversation,
        } as unknown as PersistentDataStore
        const coordinator = {
            revision: 1,
            mutationGeneration: 0,
            hasPendingPersistenceWork: false,
            flushPendingData: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
            adoptWindowedSelectedConversation: vi.fn(() => true),
            markPersistentDataDirty: vi.fn(),
            runSelectedConversationTransition: <T>(transition: () => T) =>
                transition(),
        } as unknown as WorkingSetCoordinator
        const workingSet = new ActiveWorkingSet({
            store,
            coordinator,
            getSelectedCharacterId: adapter.getSelectedCharacterId,
            getResidentCharacter: adapter.captureCharacter,
            publishCharacter: adapter.publishCharacter,
            publishCharacterSet: adapter.publishCharacterSet!,
            publishConversation: adapter.publishConversation,
            captureActivationRollback: adapter.captureActivationRollback,
            canActivateWorkingSet: () => true,
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        })

        expect(await workingSet.activateCharacter(detail.chaId)).toBe(true)
        expect(workingSet.selectedConversationMode).toBe('windowed')
        expect(readConversation).not.toHaveBeenCalled()
        const reloadBeforePromotion = get(ReloadGUIPointer)

        const lease = await workingSet.acquireCompleteConversation(
            'synthetic-production-edit',
        )
        expect(workingSet.selectedConversationMode).toBe('complete')
        expect(getDatabase().characters[0].chats[0].message).toEqual(message)
        expect(lease.session.totalMessages).toBe(message.length)
        expect(readConversation).toHaveBeenCalledOnce()
        expect(get(ReloadGUIPointer)).toBe(reloadBeforePromotion)
        const source = workingSet.activeConversationViewportSource!
        const key = source.snapshot().keyAt(0)!
        expect(source.captureMessageTarget(key)).toMatchObject({
            kind: 'session',
            absoluteIndex: 0,
            message: message[0],
        })
        lease.release()
        await vi.waitFor(() =>
            expect(workingSet.selectedConversationMode).toBe('windowed'),
        )
        expect(get(ReloadGUIPointer)).toBe(reloadBeforePromotion)
        expect(source.captureMessageTarget(key)).toBeNull()

        const nextLease = await workingSet.acquireCompleteConversation(
            'synthetic-production-second-edit',
        )
        const nextSource = workingSet.activeConversationViewportSource!
        const nextKey = nextSource.snapshot().keyAt(0)!
        expect(nextSource.captureMessageTarget(nextKey)).toMatchObject({
            kind: 'session',
            absoluteIndex: 0,
            message: message[0],
        })
        expect(readConversation).toHaveBeenCalledTimes(2)
        nextLease.release()
    })
})
