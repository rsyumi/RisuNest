// @vitest-environment node

import './tests/selectedConversationEvictionNodeDom.setup'
import 'fake-indexeddb/auto'
import { IDBKeyRange, indexedDB } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'
import {
    captureChatMessageTarget,
    queryChatMessageTargetAt,
    queryChatMessageTargetById,
    queryChatMessageTargetsByIds,
    renameCapturedBookmark,
} from '../chatMessageUi'
import { createCapturedConversationBranch } from '../chatBranchUi'
import { openChatScreenshotSourceLease } from '../chatScreenshotSourceLease'
import { createConversationOperationContext } from '../process/conversationOperationContext'
import { executeRegexPlanSync, getRegexExecutionPlan } from '../process/regexExecutionPlan'
import type { ChatScreenshotRenderContext } from '../chatScreenshotRange'
import type { Chat, Database, Message, character } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { RevisionConflictError, type WorkingSetCommit } from './persistentDataStore'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
    publishPersistentConversationReplacementToWorkingSet,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
import { createPluginDatabaseAccess } from '../plugins/pluginDatabaseAccess'

const INITIAL_MESSAGE_COUNT = 10_000
const VIEWPORT_ROW_BUDGET = 64

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function makeMessage(index: number): Message {
    return {
        role: index % 2 === 0 ? 'user' : 'char',
        data: `turn-${index.toString().padStart(5, '0')}`,
        chatId: index === 100 || index === 9_000 ? 'duplicate-anchor' : `message-${index}`,
        name: index % 97 === 0 ? `speaker-${index}` : undefined,
        saying: index % 131 === 0 ? `aside-${index}` : undefined,
    }
}

function makeConversation(): Chat {
    return {
        id: 'chat-a',
        name: 'Corpus conversation',
        note: 'synthetic 10,000-turn owner',
        localLore: [],
        fmIndex: -1,
        message: Array.from({ length: INITIAL_MESSAGE_COUNT }, (_, index) => makeMessage(index)),
    }
}

function makeDatabase(conversation: Chat): Database {
    return {
        username: 'Eviction corpus',
        botPresets: [],
        botPresetsId: 0,
        pluginCustomStorage: {},
        statics: { messages: 0 },
        aiModel: 'test-model',
        maxContext: 8_192,
        maxResponse: 128,
        promptTemplate: [{ type: 'chat', rangeStart: 0, rangeEnd: 'end' }],
        promptSettings: { trimStartNewChat: true, sendName: false },
        customPromptTemplateToggle: '', globalChatVariables: {}, mainPrompt: '',
        additionalPrompt: '', globalNote: '', jailbreak: '', jailbreakToggle: false,
        chainOfThought: false, personaPrompt: false, promptPreprocess: false,
        descriptionPrefix: '', formatingOrder: [], bias: [], outputImageModal: false,
        rememberToolUsage: false, streamingDisplayOptimizationMode: 'off',
        autoContinueMinTokens: 0, autoContinueChat: false, notification: false,
        ttsAutoSpeech: false, supaModelType: 'none', hanuraiEnable: false,
        hypav2: false, hypaV3: false,
        characters: [{
            type: 'character',
            chaId: 'char-a',
            name: 'Synthetic owner',
            firstMessage: 'Greeting',
            alternateGreetings: [],
            desc: '', personality: '', scenario: '', bias: [], additionalAssets: [],
            emotionImages: [], reloadKeys: 0, viewScreen: 'none', inlayViewScreen: false,
            supaMemory: false, utilityBot: false,
            chatPage: 0,
            chats: [conversation],
        }],
    } as unknown as Database
}

function screenshotRenderContext(owner: character): ChatScreenshotRenderContext {
    const projected = structuredClone(owner)
    projected.chats[projected.chatPage ?? 0].message = []
    return {
        character: null,
        characterName: owner.name,
        characterImageSource: '',
        characterLargePortrait: false,
        userName: 'User',
        userImageSource: '',
        userLargePortrait: false,
        moduleAssets: [],
        presetRegex: [],
        moduleRegexScripts: [],
        assetStyle: '',
        parserContext: {
            database: { characters: [projected] } as Database,
            character: projected,
            userName: 'User',
            personaPrompt: '',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
        settings: {
            autoTranslate: false,
            autoTranslateCachedOnly: false,
            translatorType: 'google',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
            assetWidth: -1,
            hideAllImages: false,
            iconSize: 100,
            zoomSize: 100,
            lineHeight: 1.25,
            dynamicAssets: false,
            dynamicAssetsEditDisplay: false,
            legacyMediaFindings: false,
            assetMaxDifference: 0.5,
        },
    }
}

async function waitForWindowed(runtime: ReturnType<typeof createPersistentDataRuntime>) {
    await vi.waitFor(() => expect(runtime.getSelectedConversationMode()).toBe('windowed'))
    expect(runtime.getActiveConversationSession()).toBeNull()
    const source = runtime.getActiveConversationViewportSource()
    expect(source).not.toBeNull()
    expect(source!.snapshot().totalMessages).toBeGreaterThan(0)
}

describe('selected conversation eviction correctness corpus', () => {
    it('matches a complete-owner oracle across forced 10,000-turn demotion cycles', async () => {
        const initialConversation = makeConversation()
        const oracle = structuredClone(initialConversation)
        let workingCopy = structuredClone(makeDatabase(initialConversation))
        workingCopy.characters.push({
            type: 'character',
            chaId: 'char-b',
            name: 'Non-target owner',
            chatPage: 0,
            chats: [{
                id: 'chat-b',
                name: 'Non-target conversation',
                note: '',
                localLore: [],
                message: [{ role: 'user', data: 'must remain unread' }],
            }],
        } as character)
        const store = new IndexedDbPersistentDataStore(
            `selected-eviction-corpus-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const initial = await store.replaceFromDatabase(workingCopy)
        const storeMaterializeDatabase = vi.spyOn(store, 'materializeDatabase')
        const storeReplaceFromDatabase = vi.spyOn(store, 'replaceFromDatabase')
        const storeReadCharacter = vi.spyOn(store, 'readCharacter')
        const selectedConversation = () => {
            const owner = workingCopy.characters[0]
            return owner.chats[owner.chatPage ?? 0]
        }
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((candidate) => candidate.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => selectedConversation()?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[workingCopy.characters[0].chatPage ?? 0] = conversation
            },
            publishConversationReplacement: (result) => {
                publishPersistentConversationReplacementToWorkingSet(workingCopy, result)
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
            conversationViewportRowBudget: VIEWPORT_ROW_BUDGET,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        await waitForWindowed(runtime)
        let expectedRevision = initial.revision

        const context = {
            captureCurrent: () => ({
                character: workingCopy.characters[0],
                conversation: selectedConversation(),
            }),
            getCurrentSession: () => runtime.getActiveConversationSession(),
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            acquirePersistentRevision: (revision: number) => store.acquireRevision(revision),
            acquireCompleteConversation: (
                reason: string,
                target = runtime.captureSelectedConversationTarget(),
            ) => runtime.acquireCompleteConversation(reason, target),
        }

        const assertSelectedWindowed = async (
            stage: string,
            conversationId: string,
            expectedConversation: Chat,
        ) => {
            await waitForWindowed(runtime)
            expect(() => selectedConversation().message).toThrow('metadata-only')
            expect(Object.keys(selectedConversation()), `${stage}: metadata shell keys`)
                .not.toContain('message')
            const { message: _expectedMessages, ...expectedMetadata } = expectedConversation
            expect({ ...selectedConversation() }, `${stage}: metadata shell`).toEqual(expectedMetadata)
            expect(runtime.captureSelectedConversationAuthority()).toMatchObject({
                characterId: 'char-a',
                conversationId,
                storeRevision: expectedRevision,
                totalMessages: expectedConversation.message.length,
            })
            const persisted = await store.readConversation('char-a', conversationId)
            expect(persisted, `${stage}: authoritative conversation`).not.toBeNull()
            expect(persisted!.revision, `${stage}: authoritative revision`).toBe(expectedRevision)
            expect(persisted!.value, `${stage}: authoritative conversation value`)
                .toEqual(expectedConversation)
            expect(
                persisted!.value.message.map((message) => message.chatId),
                `${stage}: authoritative message IDs`,
            ).toEqual(expectedConversation.message.map((message) => message.chatId))
            const viewport = runtime.getActiveConversationViewportSource()!.snapshot()
            let residentRows = 0
            for (let index = 0; index < viewport.totalMessages; index++) {
                if (viewport.rowAt(index) !== undefined) residentRows += 1
            }
            expect(residentRows, `${stage}: resident viewport rows`)
                .toBeLessThanOrEqual(VIEWPORT_ROW_BUDGET)
        }
        const assertWindowed = (stage = 'unnamed stage') =>
            assertSelectedWindowed(stage, 'chat-a', oracle)

        const profile = { profile: 'scalable-v3' as const, allowsEviction: true }
        const materializeDatabaseSnapshot = vi.fn()
        const replacePersistentDatabase = vi.fn()
        const pluginAccess = createPluginDatabaseAccess({
            store,
            flushPendingData: (reason) => runtime.flushPendingData(reason),
            getCompatibilityDatabase: () => workingCopy,
            getCompatibilityProfile: () => profile.profile,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId ?? null,
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            acquireCompleteConversation: (reason, target) =>
                runtime.acquireCompleteConversation(reason, target),
            refreshSelectedConversationAfterReplacement: (target, expectedSession) =>
                runtime.refreshSelectedConversationAfterReplacement(target, expectedSession),
            replacePersistentCompleteCharacter: (characterId, reason, mutate, options) =>
                runtime.replacePersistentCompleteCharacter(characterId, reason, mutate, options),
            replacePersistentConversation: (
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ) => runtime.replacePersistentConversation(
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ),
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: () => runtime.getNavigationGeneration(),
            applyCompatibilityDatabaseLite: vi.fn(),
            applyCompatibilityDatabase: vi.fn(),
            readPluginStorageSnapshot: vi.fn(async () => ({})),
            mutatePluginStorage: vi.fn(),
            invalidatePluginStorage: vi.fn(),
            materializeDatabaseSnapshot,
            replacePersistentDatabase,
            snapshot: structuredClone,
        })
        const detached = await pluginAccess.getChatFromIndex(0, 0, {
            pluginName: 'corpus-plugin',
            signal: new AbortController().signal,
        })
        expect(detached!.message).toHaveLength(INITIAL_MESSAGE_COUNT)
        const pluginReplacement = {
            ...detached!,
            note: 'scoped plugin replacement',
        }
        const releaseScopedCommit = deferred<void>()
        const originalCommit = store.commit.bind(store)
        const scopedCommit = vi.spyOn(store, 'commit').mockImplementationOnce(async (input) => {
            await releaseScopedCommit.promise
            return originalCommit(input)
        })
        const scopedWrite = pluginAccess.setChatToIndex(0, 0, pluginReplacement, {
            pluginName: 'corpus-plugin',
            signal: new AbortController().signal,
        })
        await vi.waitFor(() => expect(scopedCommit).toHaveBeenCalledOnce())
        expect(runtime.getSelectedConversationMode()).toBe('complete')
        expect(runtime.getActiveConversationSession()?.pinCount('compatibility')).toBe(1)
        releaseScopedCommit.resolve()
        await scopedWrite
        scopedCommit.mockRestore()
        const persistedPluginReplacement = await store.readConversation('char-a', 'chat-a')
        Object.assign(oracle, persistedPluginReplacement!.value)
        expectedRevision += 1
        await assertWindowed('scalable plugin getter and setter')
        expect(profile).toEqual({ profile: 'scalable-v3', allowsEviction: true })
        expect(materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(replacePersistentDatabase).not.toHaveBeenCalled()
        expect(storeMaterializeDatabase).not.toHaveBeenCalled()
        expect(storeReplaceFromDatabase).not.toHaveBeenCalled()
        expect(storeReadCharacter.mock.calls.some(([id]) => id === 'char-b')).toBe(false)

        const mutateComplete = async (
            reason: string,
            mutateRuntime: (session: NonNullable<ReturnType<typeof runtime.getActiveConversationSession>>) => void,
            mutateOracle: () => void,
            failure?: Error,
        ) => {
            const target = runtime.captureSelectedConversationTarget()
            const lease = await runtime.acquireCompleteConversation(reason, target)
            expect(lease.session.totalMessages).toBe(oracle.message.length)
            expect('evictionEnabled' in lease.session).toBe(false)
            mutateRuntime(lease.session)
            mutateOracle()
            lease.release()

            const originalCommit = store.commit.bind(store)
            const commitSpy = failure
                ? vi.spyOn(store, 'commit').mockRejectedValueOnce(failure)
                : null
            if (failure) {
                await expect(runtime.flushPendingData(`${reason}-failed`)).rejects.toBe(failure)
                expect(runtime.getSelectedConversationMode()).toBe('complete')
                commitSpy!.mockImplementation((commit: WorkingSetCommit) => originalCommit(commit))
            }
            await runtime.flushPendingData(`${reason}-persisted`)
            expectedRevision += 1
            commitSpy?.mockRestore()
            await assertWindowed(reason)
        }

        await mutateComplete(
            'append',
            (session) => session.append({ role: 'user', data: 'appended', chatId: 'op-append' }),
            () => { oracle.message.push({ role: 'user', data: 'appended', chatId: 'op-append' }) },
        )
        await mutateComplete(
            'edit',
            (session) => session.edit(session.locate(8_888), {
                ...session.readMessage(session.locate(8_888)),
                data: 'edited far turn',
                saying: 'edit metadata',
            }),
            () => { oracle.message[8_888] = { ...oracle.message[8_888], data: 'edited far turn', saying: 'edit metadata' } },
        )
        await mutateComplete(
            'delete',
            (session) => session.delete(session.locate(17)),
            () => { oracle.message.splice(17, 1) },
        )

        const shifted = await queryChatMessageTargetById(context, 'message-18')
        expect(shifted).toMatchObject({ absoluteIndex: 17, message: oracle.message[17] })
        const firstDuplicate = await queryChatMessageTargetById(context, 'duplicate-anchor', 'first')
        const lastDuplicate = await queryChatMessageTargetById(context, 'duplicate-anchor', 'last')
        expect(firstDuplicate).toMatchObject({ absoluteIndex: 99, message: oracle.message[99] })
        expect(lastDuplicate).toMatchObject({ absoluteIndex: 8_999, message: oracle.message[8_999] })
        await assertWindowed('shifted anchored queries')

        const branchEnd = 257
        const branchLease = await runtime.acquireCompleteConversation('branch-gateway')
        const branchTarget = captureChatMessageTarget({
            ...context,
            absoluteIndex: branchEnd,
        })
        expect(branchTarget?.kind).toBe('session')
        const branchIds = ['branch-a', 'branch-marker'][Symbol.iterator]()
        await expect(createCapturedConversationBranch({
            target: branchTarget!,
            context,
            runtime,
            createFolderOnBranch: false,
            createId: () => branchIds.next().value!,
            createBranchName: () => 'Corpus branch',
            navigateToBranch: async (id) => {
                await runtime.activateConversation(id)
                return true
            },
            pageSize: 128,
        })).resolves.toBe(true)
        branchLease.release()
        expectedRevision += 1
        const branchMarker: Message = {
            role: 'char',
            data: `{{specialcomment::branchedfrom::${oracle.id}::${oracle.name}::${oracle.message[branchEnd].chatId}::}}`,
            isComment: true,
            disabled: true,
            chatId: 'branch-marker',
        }
        const { message: _sourceMessages, ...sourceMetadata } = structuredClone(oracle)
        const branchOracle: Chat = {
            ...sourceMetadata,
            id: 'branch-a',
            name: 'Corpus branch',
            message: [
                ...structuredClone(oracle.message.slice(0, branchEnd + 1)),
                branchMarker,
            ],
        }
        await assertSelectedWindowed('persistent branch immediate', 'branch-a', branchOracle)
        const persistedBranch = await store.readConversation('char-a', 'branch-a')
        expect(persistedBranch?.value).toEqual(branchOracle)
        await runtime.activateConversation('chat-a')
        await assertWindowed('persistent branch gateway')

        const exportedBeforeTruncate = await runtime
            .materializePersistentDatabaseSnapshotWithRevision('corpus-export-before-truncate')
        expect(exportedBeforeTruncate.revision).toBe(expectedRevision)
        expect(exportedBeforeTruncate.database.characters[0].chats.find((chat) => chat.id === 'chat-a'))
            .toEqual(oracle)
        expect(exportedBeforeTruncate.database.characters[0].chats.find((chat) => chat.id === 'branch-a'))
            .toEqual(persistedBranch!.value)
        await assertWindowed('pre-truncate export')

        await mutateComplete(
            'truncate',
            (session) => session.truncate(session.locate(session.totalMessages - 2)),
            () => { oracle.message.splice(oracle.message.length - 2) },
        )
        await mutateComplete(
            'reroll',
            (session) => session.reroll(session.positionAt(session.totalMessages - 1), [{
                role: 'char',
                data: 'rerolled tail',
                chatId: 'op-reroll',
                generationInfo: { model: 'synthetic' },
            }]),
            () => oracle.message.splice(oracle.message.length - 1, 1, {
                role: 'char',
                data: 'rerolled tail',
                chatId: 'op-reroll',
                generationInfo: { model: 'synthetic' },
            }),
        )

        const bookmarkTarget = await queryChatMessageTargetById(context, 'message-9001')
        expect(bookmarkTarget?.kind).toBe('persistent')
        const bookmarkLease = await runtime.acquireCompleteConversation(
            'bookmark',
            bookmarkTarget!.kind === 'persistent' ? bookmarkTarget!.selection : null,
        )
        bookmarkLease.session.setBookmark(bookmarkLease.session.locate(bookmarkTarget!.absoluteIndex), {
            bookmarked: true,
            name: 'Far bookmark',
        })
        bookmarkLease.release()
        oracle.bookmarks = ['message-9001']
        oracle.bookmarkNames = { 'message-9001': 'Far bookmark' }
        await runtime.flushPendingData('bookmark-persisted')
        expectedRevision += 1
        await assertWindowed('bookmark')
        const renameTarget = await queryChatMessageTargetById(context, 'message-9001')
        await expect(renameCapturedBookmark(renameTarget!, context, async () => 'Renamed bookmark'))
            .resolves.toBe(true)
        oracle.bookmarkNames['message-9001'] = 'Renamed bookmark'
        await runtime.flushPendingData('bookmark-rename-persisted')
        expectedRevision += 1
        await assertWindowed('bookmark rename')

        await mutateComplete(
            'failed-save-retry',
            (session) => session.append({ role: 'user', data: 'retry retained', chatId: 'op-retry' }),
            () => { oracle.message.push({ role: 'user', data: 'retry retained', chatId: 'op-retry' }) },
            new Error('synthetic save failure'),
        )
        const conflictMessage: Message = {
            role: 'user',
            data: 'stale conflict attempt',
            chatId: 'op-conflict',
            saying: 'stable conflict evidence',
        }
        const staleLease = await runtime.acquireCompleteConversation('revision-conflict-stale')
        staleLease.session.append(structuredClone(conflictMessage))
        staleLease.release()
        const competingRoot = await store.readRoot()
        const competingCommit = await store.commit({
            expectedRevision,
            root: { ...competingRoot.value, username: 'Competing writer' },
        })
        expectedRevision = competingCommit.revision
        await expect(runtime.flushPendingData('revision-conflict-stale'))
            .rejects.toBeInstanceOf(RevisionConflictError)
        expect(runtime.getSelectedConversationMode()).toBe('complete')
        await runtime.refreshActiveWorkingSetFromStore(competingCommit.revision)
        await waitForWindowed(runtime)
        const retryLease = await runtime.acquireCompleteConversation('revision-conflict-retry')
        retryLease.session.append(structuredClone(conflictMessage))
        retryLease.release()
        oracle.message.push(structuredClone(conflictMessage))
        await runtime.flushPendingData('revision-conflict-retry')
        expectedRevision += 1
        const conflictPersisted = await store.readConversation('char-a', 'chat-a')
        expect(conflictPersisted!.value.message.filter((message) =>
            message.chatId === conflictMessage.chatId
        )).toEqual([conflictMessage])
        await assertWindowed('revision conflict exact retry')

        const regexLease = await runtime.acquireCompleteConversation('regex-operation-context')
        const regexOperation = createConversationOperationContext(
            regexLease.session,
            selectedConversation(),
        )
        expect(regexOperation.mode).toBe('compatibility')
        expect(regexOperation.chat.message).toEqual(oracle.message)
        const regexPlan = getRegexExecutionPlan([{
            comment: 'corpus regex',
            in: '^stale conflict attempt$',
            out: 'regex compatibility output',
            type: 'editoutput',
            flag: 'g',
            ableFlag: true,
        }], 'editoutput')
        const regexIndex = regexOperation.chat.message.length - 1
        regexOperation.chat.message[regexIndex] = {
            ...regexOperation.chat.message[regexIndex],
            data: executeRegexPlanSync(
                regexPlan,
                regexOperation.chat.message[regexIndex].data,
                (value) => value,
            ).data,
            saying: 'regex visited complete history',
        }
        expect(regexOperation.commit(regexLease.session)).toEqual(expect.arrayContaining([
            expect.objectContaining({ type: 'replace-range' }),
        ]))
        regexLease.release()
        oracle.message[regexIndex] = {
            ...oracle.message[regexIndex],
            data: 'regex compatibility output',
            saying: 'regex visited complete history',
        }
        await runtime.flushPendingData('regex-operation-context')
        expectedRevision += 1
        await assertWindowed('regex operation context')

        const luaLease = await runtime.acquireCompleteConversation('lua-operation-context')
        const luaOperation = createConversationOperationContext(
            luaLease.session,
            selectedConversation(),
        )
        expect(luaOperation.mode).toBe('compatibility')
        vi.doMock('../parser/parser.svelte', () => ({
            hasher: vi.fn(),
            risuChatParser: (value: string) => value,
        }))
        vi.doMock('../alert', () => ({
            alertConfirm: vi.fn(),
            alertError: vi.fn(),
            alertInput: vi.fn(),
            alertNormal: vi.fn(),
            alertSelect: vi.fn(),
        }))
        vi.doMock('../globalApi.svelte', () => ({ fetchNative: vi.fn(), readImage: vi.fn() }))
        vi.doMock('../platform', () => ({
            isTauriMobile: true,
            isNodeServer: false,
            isTauri: false,
            isMobile: false,
        }))
        vi.doMock('../tokenizer', () => ({ tokenize: vi.fn() }))
        vi.doMock('../util', () => ({
            asBuffer: vi.fn(),
            getPersonaPrompt: vi.fn(),
            getUserIcon: vi.fn(),
            getUserName: vi.fn(() => 'User'),
        }))
        vi.doMock('./database.svelte', () => ({
            getCurrentCharacter: () => workingCopy.characters[0],
            getCurrentChat: () => selectedConversation(),
            getDatabase: () => workingCopy,
            setDatabase: vi.fn(),
        }))
        vi.doMock('../stores.svelte', () => ({
            DBState: { db: workingCopy },
            ReloadChatPointer: { update: vi.fn() },
            ReloadGUIPointer: { update: vi.fn() },
            selectedCharID: { subscribe: (run: (value: number) => void) => (run(0), () => undefined) },
        }))
        vi.doMock('../process/modules', () => ({
            getModuleLorebooks: () => [],
            getModuleTriggers: () => [],
        }))
        vi.doMock('../process/files/inlays', () => ({
            getInlayAsset: vi.fn(),
            writeInlayImage: vi.fn(),
        }))
        vi.doMock('../process/lorebook.svelte', () => ({
            loadLoreBookV3PromptFromCompatibilitySnapshot: vi.fn(),
        }))
        vi.doMock('../process/memory/hypamemory', () => ({ HypaProcesser: vi.fn() }))
        vi.doMock('../process/request/request', () => ({ requestChatData: vi.fn() }))
        vi.doMock('../process/stableDiff', () => ({ generateAIImage: vi.fn() }))
        const { readFile } = await import('node:fs/promises')
        const { resolve } = await import('node:path')
        const jsonLuaSource = await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8')
        const originalFetch = globalThis.fetch
        vi.stubGlobal('fetch', vi.fn(async () => new Response(jsonLuaSource, { status: 200 })))
        const nodeDomGlobals = {
            window: globalThis.window,
            document: globalThis.document,
            navigator: globalThis.navigator,
            location: globalThis.location,
        }
        for (const key of Object.keys(nodeDomGlobals)) {
            Reflect.deleteProperty(globalThis, key)
        }
        const { runScripted } = await import('../process/scriptings')
        const luaResult = await runScripted(`
            listenEdit('editInput', function(id, value, meta)
                addChat(id, 'user', 'Lua compatibility output')
                return false
            end)
        `, {
            char: workingCopy.characters[0],
            mode: 'editInput',
            operationContext: luaOperation,
        })
        expect(luaResult.stopSending).toBe(true)
        const luaMessage = luaOperation.chat.message.at(-1)!
        luaMessage.chatId = 'op-lua'
        luaMessage.saying = 'Lua visited complete history'
        luaOperation.commit(luaLease.session)
        luaLease.release()
        oracle.message.push(structuredClone(luaMessage))
        await runtime.flushPendingData('lua-operation-context')
        expectedRevision += 1
        await assertWindowed('Lua operation context')
        for (const [key, value] of Object.entries(nodeDomGlobals)) {
            Object.defineProperty(globalThis, key, { configurable: true, value })
        }
        vi.stubGlobal('fetch', originalFetch)
        for (const moduleId of [
            '../parser/parser.svelte',
            '../alert',
            '../globalApi.svelte',
            '../platform',
            '../tokenizer',
            '../util',
            './database.svelte',
            '../stores.svelte',
            '../process/modules',
            '../process/files/inlays',
            '../process/lorebook.svelte',
            '../process/memory/hypamemory',
            '../process/request/request',
            '../process/stableDiff',
        ]) vi.doUnmock(moduleId)
        vi.resetModules()

        const generationDBState = {
            get db() { return workingCopy },
            set db(value: Database) { workingCopy = value },
        }
        vi.doMock('../stores.svelte', () => ({
            DBState: generationDBState,
            selectedCharID: writable(0),
            ReloadGUIPointer: { update: vi.fn() },
        }))
        vi.doMock('./persistentDataRuntime.svelte', () => ({
            acquireDestructiveReplacementFence: vi.fn(),
            acknowledgeGenerationCompletion: () => runtime.acknowledgeGenerationCompletion(),
            capturePersistentMutationToken: vi.fn(),
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            acquireCompleteConversation: (reason: string, target: never) =>
                runtime.acquireCompleteConversation(reason, target),
            getActiveConversationSession: () => runtime.getActiveConversationSession(),
            getPersistentDataRuntime: () => runtime,
            invalidateActiveConversationSession: () => runtime.invalidateActiveConversationSession(),
        }))
        vi.doMock('../process/request/request', () => ({
            requestChatData: vi.fn(async () => ({
                type: 'streaming',
                result: new ReadableStream({
                    start(controller) {
                        controller.enqueue({ response: 'Generation compatibility output' })
                        controller.close()
                    },
                }),
            })),
        }))
        vi.doMock('../tokenizer', () => ({
            ChatTokenizer: class {
                async tokenizeChat() { return 1 }
                async tokenizeChats(chats: unknown[]) { return chats.length }
            },
            tokenize: vi.fn(async () => 1), tokenizeNum: vi.fn(async () => []),
        }))
        vi.doMock('../../lang', () => ({ language: { errors: {}, otherUserRequesting: '' } }))
        vi.doMock('../alert', () => ({ alertError: vi.fn(), alertToast: vi.fn() }))
        vi.doMock('../parser/chatML', () => ({ parseChatML: (value: string) => value }))
        vi.doMock('../parser/parser.svelte', () => ({ risuChatParser: (value: string) => value }))
        vi.doMock('../util', () => ({
            checkNullish: (value: unknown) => value == null,
            findCharacterbyId: () => workingCopy.characters[0],
            getAuthorNoteDefaultText: () => '', getPersonaPrompt: () => '', getUserName: () => 'User',
            isLastCharPunctuation: () => true, trimUntilPunctuation: (value: string) => value,
            parseToggleSyntax: () => [], prebuiltAssetCommand: '',
        }))
        vi.doMock('../process/scripts', () => ({
            createPromptScriptOperationScope: () => ({
                assertOwnerCurrent: vi.fn(), adoptMessageId: vi.fn(),
                parse: (_char: unknown, value: string) => value,
                finish: vi.fn(), finishAfterError: vi.fn(), release: vi.fn(),
            }),
            processScript: vi.fn(async (_char: unknown, value: string) => value),
            processScriptFull: vi.fn(async (_char: unknown, value: string) => ({ data: value, emoChanged: false })),
            risuChatParser: (value: string) => value, resetScriptCache: vi.fn(),
        }))
        vi.doMock('../process/triggers', () => ({
            runTrigger: vi.fn(async (_char: unknown, mode: string, arg: { chat: Chat }) =>
                mode === 'start' ? null : { chat: arg.chat }),
        }))
        vi.doMock('../process/modules', () => ({
            getModuleAssets: () => [], getModuleToggles: () => '', moduleUpdate: vi.fn(),
        }))
        for (const [id, exports] of [
            ['../process/lorebook.svelte', { loadLoreBookV3Prompt: vi.fn(async () => ({ actives: [] })) }],
            ['../process/templates/templates', { prebuiltNAIpresets: [], prebuiltPresets: { OAI: { mainPrompt: '', jailbreak: '' } } }],
            ['../process/exampleMessages', { exampleMessage: () => [] }],
            ['../process/tts', { sayTTS: vi.fn() }],
            ['../process/memory/supaMemory', { supaMemory: vi.fn() }],
            ['../process/group', { groupOrder: (value: unknown) => value }],
            ['../process/memory/hypamemory', { HypaProcesser: class {} }],
            ['../process/embedding/addinfo', { additionalInformations: vi.fn(async () => '') }],
            ['../process/files/inlays', { getInlayAsset: vi.fn(async () => null) }],
            ['../process/models/modelString', { getGenerationModelString: () => 'test-model' }],
            ['../process/inlayScreen', { runInlayScreen: (_char: unknown, data: string) => ({ text: data }) }],
            ['../process/transformers', { runImageEmbedding: vi.fn() }],
            ['../process/memory/hanuraiMemory', { hanuraiMemory: vi.fn() }],
            ['../process/memory/hypav2', { hypaMemoryV2: vi.fn() }],
            ['../process/memory/hypav3', { hypaMemoryV3: vi.fn() }],
            ['../process/scriptings', { runLuaEditTrigger: vi.fn(async (_c: unknown, _m: string, value: unknown) => value) }],
            ['../globalApi.svelte', { readImage: vi.fn() }],
            ['../plugins/plugins.svelte', { pluginV2: { chatOutput: new Set() } }],
            ['../process/presetChain', { activatePresetChainForRequest: vi.fn() }],
        ] as const) vi.doMock(id, () => exports)
        vi.doMock('../model/modellist', () => ({ getModelInfo: () => ({ flags: [] }), LLMFlags: {} }))
        vi.doMock('../sync/multiuser', () => ({
            connectionOpen: false, peerRevertChat: vi.fn(), peerSafeCheck: vi.fn(async () => true),
            peerSync: vi.fn(),
        }))
        const generationBefore = oracle.message.length
        const { sendChat } = await import('../process/index.svelte')
        const generationLog = vi.spyOn(console, 'log').mockImplementation(() => undefined)
        await expect(sendChat()).resolves.toBe(true)
        generationLog.mockRestore()
        expectedRevision += 1
        const generatedConversation = await store.readConversation('char-a', 'chat-a')
        const generatedMessage = generatedConversation!.value.message[generationBefore]
        expect(generatedMessage.data).toBe('Generation compatibility output')
        oracle.message.push(structuredClone(generatedMessage))
        oracle.isStreaming = generatedConversation!.value.isStreaming
        oracle.lastMemory = generatedConversation!.value.lastMemory
        await assertWindowed('public generation gateway')
        vi.resetModules()

        const cbsLease = await runtime.acquireCompleteConversation('cbs-operation-context')
        const cbsOperation = createConversationOperationContext(
            cbsLease.session,
            selectedConversation(),
        )
        const cbsCallbacks = new Map<string, import('../cbs').RegisterCallback>()
        vi.doMock('../stores.svelte', () => ({ CurrentTriggerIdStore: writable(null) }))
        const { defaultCBSRegisterArg, registerCBS } = await import('../cbs')
        registerCBS({
            ...defaultCBSRegisterArg,
            getDatabase: () => cbsOperation.createDatabaseView(workingCopy),
            getSelectedCharID: () => 0,
            registerFunction: ({ name, alias, callback }) => {
                if (callback === 'doc_only') return
                for (const key of [name, ...alias]) cbsCallbacks.set(key, callback)
            },
        })
        const cbsDatabase = cbsOperation.createDatabaseView(workingCopy)
        expect(cbsCallbacks.get('previouscharchat')!('', {
            chatID: -1,
            db: cbsDatabase,
            chara: cbsDatabase.characters[0],
            selectedCharacterId: 'char-a',
            rmVar: false,
            cbsConditions: {},
        } as never, [], null)).toBe('Generation compatibility output')
        cbsOperation.release()
        cbsLease.release()
        await assertWindowed('CBS operation context')
        vi.doUnmock('../stores.svelte')
        vi.resetModules()

        const triggerLease = await runtime.acquireCompleteConversation('trigger-operation-context')
        const triggerOperation = createConversationOperationContext(
            triggerLease.session,
            selectedConversation(),
        )
        expect(triggerOperation.mode).toBe('compatibility')
        vi.doUnmock('../process/triggers')
        vi.doUnmock('../util')
        const triggerStoreState = { DBState: { db: workingCopy } }
        vi.doMock('../stores.svelte', () => ({
            ...triggerStoreState,
            CurrentTriggerIdStore: writable(null),
            selectedCharID: writable(0),
        }))
        vi.doMock('./database.svelte', () => ({ getDatabase: () => workingCopy }))
        vi.doMock('./persistentDataRuntime.svelte', () => ({
            acquireDestructiveReplacementFence: vi.fn(),
            capturePersistentMutationToken: vi.fn(),
            getPersistentDataRuntime: vi.fn(),
            peekActiveConversationSession: () => triggerLease.session,
        }))
        vi.doMock('../process/modules', () => ({ getModuleTriggers: () => [] }))
        vi.doMock('../tokenizer', () => ({ tokenize: vi.fn(async () => 0) }))
        vi.doMock('../parser/parser.svelte', () => ({ risuChatParser: (value: string) => value }))
        vi.doMock('../process/command', () => ({ processMultiCommand: vi.fn() }))
        vi.doMock('../process/request/request', () => ({ requestChatData: vi.fn() }))
        vi.doMock('../process/stableDiff', () => ({ generateAIImage: vi.fn() }))
        vi.doMock('../process/files/inlays', () => ({ writeInlayImage: vi.fn() }))
        const triggerCharacter = workingCopy.characters[0] as character
        triggerCharacter.triggerscript = [{
            comment: 'corpus trigger',
            type: 'manual',
            conditions: [],
            effect: [{ type: 'impersonate', role: 'user', value: 'Trigger compatibility output' }],
        }] as never
        triggerCharacter.customscript = []
        triggerCharacter.defaultVariables = ''
        const { runTrigger } = await import('../process/triggers')
        const triggerResult = await runTrigger(triggerCharacter, 'manual', {
            chat: triggerOperation.chat,
            manualName: 'corpus trigger',
            conversationOperation: triggerOperation,
        })
        expect(triggerResult?.chat).toBe(triggerOperation.chat)
        const triggerMessage = triggerOperation.chat.message.at(-1)!
        triggerMessage.chatId = 'op-trigger'
        triggerMessage.saying = 'Trigger visited complete history'
        triggerOperation.commit(triggerLease.session)
        triggerLease.release()
        oracle.message = JSON.parse(JSON.stringify(oracle.message)) as Message[]
        oracle.message.push(structuredClone(triggerMessage))
        await runtime.flushPendingData('trigger-operation-context')
        expectedRevision += 1
        await assertWindowed('Trigger operation context')
        delete triggerCharacter.triggerscript
        delete triggerCharacter.customscript
        delete triggerCharacter.defaultVariables
        for (const moduleId of [
            '../stores.svelte',
            './database.svelte',
            './persistentDataRuntime.svelte',
            '../process/modules',
            '../tokenizer',
            '../parser/parser.svelte',
            '../process/command',
            '../process/request/request',
            '../process/stableDiff',
            '../process/files/inlays',
        ]) vi.doUnmock(moduleId)
        vi.resetModules()

        const screenshot = await openChatScreenshotSourceLease({
            characterId: 'char-a',
            chatId: 'chat-a',
            renderContext: screenshotRenderContext(workingCopy.characters[0] as character),
        }, runtime)
        const screenshotJob = await screenshot.createJob(9_501, 9_502)
        expect(screenshotJob.messages).toEqual(oracle.message.slice(9_500, 9_502))
        await screenshot.close()
        await assertWindowed('screenshot source lease')

        const search = await queryChatMessageTargetsByIds(context, [
            // Keep a mix of near, far, and generated IDs.
            'message-3',
            'message-9001',
            'op-trigger',
        ])
        expect(search.map((target) => target.message)).toEqual([
            oracle.message.find((message) => message.chatId === 'message-3'),
            oracle.message.find((message) => message.chatId === 'message-9001'),
            oracle.message.find((message) => message.chatId === 'op-trigger'),
        ])
        await assertWindowed('anchored search')

        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        expect(runtime.getActiveConversationSession()).toBeNull()
        vi.doMock('../stores.svelte', () => ({
            DBState: { db: workingCopy },
            selectedCharID: writable(0),
        }))
        vi.doMock('./persistentDataRuntime.svelte', () => ({
            acquireDestructiveReplacementFence: vi.fn(),
            acquireCompleteConversation: (
                reason: string,
                target: ReturnType<typeof runtime.captureSelectedConversationTarget>,
            ) => runtime.acquireCompleteConversation(reason, target),
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            capturePersistentMutationToken: vi.fn(),
            getPersistentDataRuntime: () => runtime,
            peekActiveConversationSession: () => runtime.getActiveConversationSession(),
        }))
        vi.doMock('../process/scripts', () => ({
            processScriptFull: vi.fn(),
            risuChatParser: (value: string) => value,
        }))
        vi.doMock('../alert', () => ({ alertConfirm: vi.fn() }))
        vi.doMock('../../lang', () => ({ language: { hypaV3Modal: { unclassified: '' } } }))
        const { captureCurrentHypaMessageById } = await import(
            '../../lib/Others/HypaV3Modal/utils'
        )
        const { createMetadataOnlySelectedConversation } = await import(
            './selectedConversationLifecycle'
        )
        const hypaOwner = workingCopy.characters[0]
        const hypaChatIndex = hypaOwner.chatPage ?? 0
        const runtimeMetadataShell = hypaOwner.chats[hypaChatIndex]
        hypaOwner.chats[hypaChatIndex] = createMetadataOnlySelectedConversation(
            runtimeMetadataShell,
        )
        const hypaRevisionSpy = vi.spyOn(store, 'acquireRevision')
        const hypaCompleteSpy = vi.spyOn(runtime, 'acquireCompleteConversation')
        const hypa = await captureCurrentHypaMessageById('message-8888')
        hypaOwner.chats[hypaChatIndex] = runtimeMetadataShell
        expect(hypa?.kind).toBe('persistent')
        expect(hypa?.message).toEqual(oracle.message.find((message) => message.chatId === 'message-8888'))
        expect(hypaRevisionSpy).toHaveBeenCalledWith(expectedRevision)
        expect(hypaCompleteSpy).not.toHaveBeenCalled()
        hypaRevisionSpy.mockRestore()
        hypaCompleteSpy.mockRestore()
        expect(runtime.getActiveConversationSession()).toBeNull()
        await assertWindowed('Hypa pinned anchor lookup')
        for (const moduleId of [
            '../stores.svelte',
            './persistentDataRuntime.svelte',
            '../process/scripts',
            '../alert',
            '../../lang',
        ]) vi.doUnmock(moduleId)
        vi.resetModules()

        const exported = await runtime.materializePersistentDatabaseSnapshotWithRevision('export')
        expect(exported.revision).toBe(expectedRevision)
        const exportedOwner = exported.database.characters[0].chats.find((chat) => chat.id === 'chat-a')!
        expect(exportedOwner).toEqual(oracle)
        await assertWindowed('authoritative export')

        for (const moduleId of [
            '../plugins/plugins.svelte',
            '../globalApi.svelte',
            '../alert',
            '../util',
            '../../lang',
            '../stores.svelte',
        ]) vi.doUnmock(moduleId)
        vi.resetModules()
        vi.doMock('./persistentDataRuntime.svelte', () => ({
            acquireDestructiveReplacementFence: vi.fn(),
            capturePersistentMutationToken: vi.fn(),
            getPersistentDataRuntime: () => runtime,
            getPersistentNavigationGeneration: () => runtime.getNavigationGeneration(),
            materializeMaximumCompatibilityWorkingSet: () =>
                runtime.materializeMaximumCompatibilityWorkingSet(),
            mutatePersistentPluginStorage: (
                reason: string,
                mutations: never,
            ) => runtime.mutatePersistentPluginStorage(reason, mutations),
            releaseInactiveWorkingSet: (
                canRelease?: () => boolean | Promise<boolean>,
                isCurrent?: () => boolean,
            ) => runtime.releaseInactiveWorkingSet(canRelease, isCurrent),
            replacePersistentDatabase: (
                database: Database,
                reason: string,
                options: never,
            ) => runtime.replacePersistentDatabase(database, reason, options),
        }))
        const pluginStores = await import('../stores.svelte')
        pluginStores.DBState.db = workingCopy
        pluginStores.selectedCharID.set(0)
        const { getV2PluginAPIs, pluginCompatibility } = await import('../plugins/plugins.svelte')
        await pluginCompatibility.transition('maximum-compatibility')
        pluginStores.DBState.db = workingCopy
        const livePluginDatabase = getV2PluginAPIs().getDatabase() as Database
        expect(livePluginDatabase).not.toBe(workingCopy)
        const livePluginConversation = livePluginDatabase.characters[0].chats
            .find((chat: Chat) => chat.id === 'chat-a')!
        expect(livePluginConversation.message).toEqual(oracle.message)
        const pluginMessage = {
            role: 'user',
            data: 'plugin compatibility append',
            chatId: 'op-plugin-v2.1',
            saying: 'live proxy compatibility',
        } as Message
        livePluginConversation.message.push(pluginMessage)
        oracle.message.push(pluginMessage)
        await pluginCompatibility.transition('scalable-v3')
        expectedRevision += 1
        await runtime.initializeActiveWorkingSet(workingCopy)
        await assertWindowed('Plugin API v2.1 live Proxy')
        vi.doUnmock('./persistentDataRuntime.svelte')

        const finalPersisted = await store.readConversation('char-a', 'chat-a')
        expect(finalPersisted).not.toBeNull()
        expect(finalPersisted!.revision).toBe(expectedRevision)
        expect(finalPersisted!.value).toEqual(oracle)
        expect(finalPersisted!.value.message.map((message) => message.chatId))
            .toEqual(oracle.message.map((message) => message.chatId))
        const finalSource = runtime.getActiveConversationViewportSource()!
        await finalSource.ensureRange({
            startIndex: 9_000,
            limit: 256,
            reason: 'viewport',
        })
        const snapshot = finalSource.snapshot()
        let residentRows = 0
        for (let index = 0; index < snapshot.totalMessages; index++) {
            if (snapshot.rowAt(index) !== undefined) residentRows += 1
        }
        expect(residentRows).toBeLessThanOrEqual(VIEWPORT_ROW_BUDGET)
        expect(runtime.getActiveConversationSession()).toBeNull()
    }, 60_000)
})
