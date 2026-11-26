import type { ConversationWindowQuery } from './storage/persistentDataStore'
import {
    classifyChatParserHistory,
    createLiveChatParserIndirections,
    planChatParserHistory,
    type ChatParserHistoryClassificationInput,
} from './chatParserHistory'
import { captureRevisionAdvance, collectLiveChatParserUnsafeDependencies } from './selectedConversationLiveParserProjection'
import {
    getCurrentCharacter,
    getCurrentChat,
    getDatabase,
    type character,
    type Database,
    type Message,
} from './storage/database.svelte'
import type { PersistentRevisionLease } from './storage/persistentDataStore'
import type { simpleCharacterArgument } from './parser/parser.svelte'
import { getPersistentDataRuntime } from './storage/persistentDataRuntime.svelte'
import type { PersistentDataRuntime } from './storage/persistentDataRuntime'
import { getModuleAssets, getModuleLorebooks, getModuleRegexScripts, getModules, getModuleTriggers } from './process/modules'
import { pluginV2 } from './plugins/plugins.svelte'
import { findCharacterbyId, getPersonaPrompt, getUserName, parseKeyValue } from './util'
import { createChatParserDependencyStamp } from './chatRenderIdentity'
import rfdc from 'rfdc'
import { cloneConversationMetadata, isMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
import { captureResidentCbsHistory, prepareCbsHistory, type CbsHistoryReader } from './prepareCbsHistory'
import type { BoundedLiveChatParserProjection } from './selectedConversationLiveParserProjection'
import { CbsHistoryDependencies, hasCbsHistoryChanges, type CbsHistoryAccess } from './cbsHistoryAccess'
import type { ConversationViewportSnapshot } from './conversationViewportSource'
export { createLiveChatParserIndirections } from './chatParserHistory'

export interface LiveDisplayParserLease {
    projection?: BoundedLiveChatParserProjection
    release(): void
}

type DisplayCharacter = character | simpleCharacterArgument | string | null
type DisplayRuntime = Pick<
    PersistentDataRuntime,
    'captureSelectedConversationTarget' | 'acquireCompleteConversation'
>
interface LiveDisplayParserInputs extends ChatParserHistoryClassificationInput {
    requiresCompleteConversation?: boolean
    triggerStamp?: string | null
    variableStamp?: string | null
}

export function createLiveChatParserSource(
    database: Database,
    character: Exclude<DisplayCharacter, string | null>,
    moduleRegex: ReturnType<typeof getModuleRegexScripts>,
) {
    return {
        guiHTML: database.theme === 'customHTML' ? database.guiHTML : '',
        // Regex inputs and results are prepared at their actual CBS pass.
        regexStamp: createChatParserDependencyStamp({ chaId: character.chaId, customscript: [
            ...(database.presetRegex ?? []), ...(character.customscript ?? []), ...moduleRegex,
        ] }),
    }
}

export function captureLiveDisplayParserInputs(
    source: unknown,
    renderCharacter: DisplayCharacter,
): LiveDisplayParserInputs {
    const database = getDatabase()
    const selectedCharacter = getCurrentCharacter()
    const conversation = getCurrentChat()
    const character =
        typeof renderCharacter === 'string'
            ? findCharacterbyId(renderCharacter)
            : (renderCharacter ?? selectedCharacter)
    if (!selectedCharacter || !conversation || !character) return { source }
    const moduleRegex = getModuleRegexScripts()
    const moduleTriggers = getModuleTriggers()
    const triggers =
        [...(character.triggerscript ?? []), ...moduleTriggers]
    return {
        source: [source, createLiveChatParserSource(database, character, moduleRegex)],
        indirections: createLiveChatParserIndirections(
            database,
            selectedCharacter,
            conversation,
            getPersonaPrompt(),
        ),
        // Keep live script edits reactive without treating script source as CBS
        // history, or depending on lowLevelAccess which Lua execution resets.
        triggerStamp: createChatParserDependencyStamp({
            chaId: character.chaId,
            triggerscript: triggers.map(({ comment, type, conditions, effect }) => ({
                comment,
                type,
                conditions,
                effect,
            })),
        }),
        variableStamp: JSON.stringify({
            scriptstate: conversation.scriptstate,
            globalChatVariables: database.globalChatVariables,
            GLGlobalVariables: conversation.GLGlobalVariables,
            defaultVariables: selectedCharacter.defaultVariables,
            templateDefaultVariables: database.templateDefaultVariables,
            userName: getUserName(),
        }),
        unsafeDependencies: collectLiveChatParserUnsafeDependencies({
            triggers,
            pluginV2EditDisplay: pluginV2.editdisplay.size > 0,
            regexScripts: [
                ...(database.presetRegex ?? []),
                ...(character.customscript ?? []),
                ...moduleRegex,
            ],
        }),
    }
}

export function createLiveDisplayParserLeaseAcquirer(dependencies: {
    runtime(): DisplayRuntime
    classify(source: unknown, character: DisplayCharacter): LiveDisplayParserInputs
    prepare?(inputs: LiveDisplayParserInputs, chatID: number, signal: AbortSignal, firstPass?: string, onStale?: () => void): Promise<LiveDisplayParserLease>
}) {
    return async (request: {
        source: unknown
        character: DisplayCharacter
        signal: AbortSignal
        chatID?: number
        parseFirst?: boolean
        onStale?: () => void
    }): Promise<LiveDisplayParserLease | null> => {
        request.signal.throwIfAborted()
        const inputs = dependencies.classify(request.source, request.character)
        const classification = dependencies.prepare ? planChatParserHistory(inputs) : classifyChatParserHistory(inputs)
        if (dependencies.prepare && !inputs.requiresCompleteConversation && !classification.requiresFullHistory) {
            return dependencies.prepare(inputs, request.chatID ?? -1, request.signal,
                request.parseFirst && typeof request.source === 'string' ? request.source : undefined, request.onStale)
        }
        if (
            !inputs.requiresCompleteConversation &&
            !classification.requiresFullHistory &&
            classification.absoluteMessageIndices.length === 0
        )
            return null
        const runtime = dependencies.runtime()
        const target = runtime.captureSelectedConversationTarget()
        if (!target) throw new Error('Live display has no selected conversation')
        const lease = await runtime.acquireCompleteConversation('live-display-parser', target)
        let released = false
        const release = () => {
            if (released) return
            released = true
            lease.release()
        }
        try {
            request.signal.throwIfAborted()
            const current = runtime.captureSelectedConversationTarget()
            if (
                !current ||
                current.characterId !== target.characterId ||
                current.conversationId !== target.conversationId ||
                current.navigationGeneration !== target.navigationGeneration ||
                lease.target.characterId !== target.characterId ||
                lease.target.conversationId !== target.conversationId ||
                lease.target.navigationGeneration !== target.navigationGeneration ||
                current.storeRevision !== lease.target.storeRevision
            ) {
                throw new Error('Live display conversation changed during preparation')
            }
            return { release }
        } catch (error) {
            release()
            throw error
        }
    }
}

export const acquireLiveDisplayParserLease = createLiveDisplayParserLeaseAcquirer({
    runtime: () => getPersistentDataRuntime(),
    classify: captureLiveDisplayParserInputs,
    prepare: prepareLiveDisplayParserProjection,
})

async function prepareLiveDisplayParserProjection(inputs: LiveDisplayParserInputs, chatID: number, signal: AbortSignal, firstPass?: string, onStale?: () => void): Promise<LiveDisplayParserLease> {
    const clone = rfdc()
    const runtime = getPersistentDataRuntime()
    const target = runtime.captureSelectedConversationTarget()
    const original = getCurrentCharacter()
    const originalChat = getCurrentChat()
    if (!target || !original || !originalChat) throw new Error('Live display has no selected conversation')
    const character = clone({ ...original, chats: [{ ...cloneConversationMetadata(originalChat), message: [] }], chatPage: 0 })
    const database = clone({ ...getDatabase(), characters: [character] })
    database.characters[0] = character
    const chatVariables = Object.fromEntries([
        ...parseKeyValue(database.templateDefaultVariables ?? ''),
        ...parseKeyValue(character.defaultVariables ?? ''),
    ])
    for (const [key, value] of Object.entries(originalChat.scriptstate ?? {})) chatVariables[key.replace(/^\$/, '')] = String(value)
    const globalChatVariables = { ...database.globalChatVariables }
    for (const [key, value] of Object.entries(originalChat.GLGlobalVariables ?? {})) {
        if (value && value !== 'null') globalChatVariables[key] = value
    }
    const context = {
        presetRegex: database.presetRegex ?? [], moduleRegexScripts: clone(getModuleRegexScripts()),
        moduleAssets: clone(getModuleAssets()), dynamicAssets: database.dynamicAssets,
        dynamicAssetsEditDisplay: database.dynamicAssetsEditDisplay,
        parserContext: { database, character, selectedCharID: 0, chatVariables, globalChatVariables,
            userName: getUserName(), personaPrompt: getPersonaPrompt(), modules: clone(getModules()),
            moduleLorebooks: clone(getModuleLorebooks()), currentTime: Date.now() },
    }
    let plan = planChatParserHistory(inputs)
    const authority = runtime.captureSelectedConversationAuthority()
    let source = runtime.getActiveConversationViewportSource()
    let version = source?.snapshot().version
    let historyVersion = source?.snapshot().historyVersion
    const revisionAdvanced = captureRevisionAdvance(runtime, target)
    const historyDependencies = new CbsHistoryDependencies()
    let completeLease: Awaited<ReturnType<typeof runtime.acquireCompleteConversation>> | undefined
    let released = false
    const release = () => { released = true; completeLease?.release(); completeLease = undefined; signal.removeEventListener('abort', release) }
    signal.addEventListener('abort', release, { once: true })
    const historyChanged = () => !!source && (source.snapshot().version !== version || hasCbsHistoryChanges(source.snapshot(), historyVersion, historyDependencies))
    const assertCurrent = () => {
        signal.throwIfAborted()
        if (released) throw new Error('Live display lease is closed')
        const current = runtime.captureSelectedConversationTarget()
        if (!current || current.characterId !== target.characterId || current.conversationId !== target.conversationId
            || current.navigationGeneration !== target.navigationGeneration || source?.snapshot().version !== version
            || runtime.getActiveConversationViewportSource() !== source || historyChanged()) {
            throw new Error('Live display conversation changed during preparation')
        }
    }
    const totalMessages = isMetadataOnlySelectedConversation(originalChat) ? authority!.totalMessages : originalChat.message.length
    const prepareComplete = async (plan: ReturnType<typeof planChatParserHistory>, index: number) => {
        assertCurrent()
        if (!completeLease) {
            const acquired = await runtime.acquireCompleteConversation('live-display-parser', target)
            if (released) { acquired.release(); signal.throwIfAborted(); throw new Error('Live display lease is closed') }
            completeLease = acquired
            source = runtime.getActiveConversationViewportSource()
            version = source?.snapshot().version
            historyVersion = source?.snapshot().historyVersion
        }
        assertCurrent()
        const messages = getCurrentChat().message
        if (messages.length !== totalMessages) throw new Error('Live display history changed during promotion')
        return captureResidentCbsHistory(messages, plan, index, historyDependencies)
    }
    const preparedRows = new Map<number, Message>()
    // Every CBS pass prepares its own input, so take the revision lease and
    // copy resident rows only when a pass needs a row it does not yet have.
    const createWindowedReader = () => {
        let resident: Map<number, Message> | undefined
        let lease: Promise<PersistentRevisionLease> | undefined
        const acquire = () => lease ??= runtime.store.acquireRevision(authority!.storeRevision)
        const storedReader = {
            readConversationWindow: async (query: ConversationWindowQuery) => (await acquire()).readConversationWindow(query),
            readConversationMessageMetadataWindow: async (query: ConversationWindowQuery) => (await acquire()).readConversationMessageMetadataWindow!(query),
        }
        const windowReader = source?.captureProjectedReader?.(storedReader) ?? storedReader
        const reader: CbsHistoryReader = {
            revision: authority!.storeRevision,
            readResidentMessage: (index) => preparedRows.get(index) ?? (resident ??= new Map(
                (source?.captureResidentRows?.() ?? []).map((row) => [row.absoluteIndex, clone(row.message)]))).get(index),
            readRoleWindow: runtime.store.messageMetadataReadMode === 'projected' && authority!.sessionVersion === authority!.persistedSessionVersion
                ? (query) => windowReader.readConversationMessageMetadataWindow!(query) : undefined,
            readConversationWindow: async (query) => {
                const result = await windowReader.readConversationWindow(query)
                return result ? { ...result, value: { ...result.value, totalMessages } } : null
            },
        }
        return { reader, release: async () => { await lease?.then((acquired) => acquired.release(), () => {}) } }
    }
    const prepareWindowed = async (plan: ReturnType<typeof planChatParserHistory>, index: number, reader: CbsHistoryReader) => {
        const access = plan.requiresFullHistory ? await prepareComplete(plan, index) : await prepareCbsHistory({
            characterId: target.characterId, conversationId: target.conversationId, dependencies: historyDependencies,
            totalMessages, chatID: index, plan, signal, assertCurrent, reader })
        for (const [row, message] of access.preparedRows()) preparedRows.set(row, message)
        return access
    }
    let historyAccess: CbsHistoryAccess
    let parsedSource: string | undefined
    const parseFirst = async () => {
        if (firstPass === undefined) return
        const { risuChatParser } = await import('./parser/parser.svelte')
        parsedSource = risuChatParser(firstPass, { ...context.parserContext, db: database, chara: character, chatID, historyAccess })
        historyAccess.assertComplete()
        plan = planChatParserHistory({ ...inputs, source: [inputs.source, parsedSource] })
    }
    try {
        if (!isMetadataOnlySelectedConversation(originalChat)) {
            historyAccess = captureResidentCbsHistory(originalChat.message, plan, chatID, historyDependencies)
            if (firstPass !== undefined) {
                await parseFirst()
                assertCurrent()
                historyAccess = captureResidentCbsHistory(originalChat.message, plan, chatID, historyDependencies)
            }
        } else {
            const windowed = createWindowedReader()
            try {
                historyAccess = await prepareWindowed(plan, chatID, windowed.reader)
                if (firstPass !== undefined) {
                    await parseFirst()
                    historyAccess = await prepareWindowed(plan, chatID, windowed.reader)
                }
            } finally { await windowed.release() }
        }
        assertCurrent()
        const prepareHistory = async (text: string, index: number, complete: boolean) => {
            try {
                assertCurrent()
                const plan = planChatParserHistory({ source: text, indirections: inputs.indirections,
                    unsafeDependencies: complete ? ['regex-reparse'] : [] })
                if (plan.requiresFullHistory || completeLease) return await prepareComplete(plan, index)
                if (!isMetadataOnlySelectedConversation(originalChat)) return captureResidentCbsHistory(originalChat.message, plan, index, historyDependencies)
                const windowed = createWindowedReader()
                try {
                    return await prepareWindowed(plan, index, windowed.reader)
                } finally { await windowed.release() }
            } catch (error) {
                // A later pass can depend on a row that changed before it was read,
                // which selection updates skipped, so the owner has to prepare again.
                // Notify after this parse call stack, which may be inside a derived.
                if (!released && !signal.aborted && (historyChanged() || revisionAdvanced())) queueMicrotask(() => {
                    if (!released && !signal.aborted) onStale?.()
                })
                throw error
            }
        }
        return { release, projection: { kind: 'bounded', characterId: target.characterId, conversationId: target.conversationId,
            revision: target.storeRevision, totalMessages, chatID, projectedChatID: chatID, historyOffset: 0, messages: [],
            parsedSource, context: { ...context, parserContext: { ...context.parserContext, historyAccess, prepareHistory } } } }
    } catch (error) {
        if (!signal.aborted && (historyChanged() || revisionAdvanced())) queueMicrotask(() => {
            if (!signal.aborted) onStale?.()
        })
        release()
        throw error
    }
}

export function captureLiveDisplayParserSelection(): string {
    const target = getPersistentDataRuntime().captureSelectedConversationTarget()
    return target
        ? JSON.stringify([target.characterId, target.conversationId, target.navigationGeneration])
        : ''
}

export function subscribeLiveDisplayParserSelection(
    listener: (identity: string, historyRevision?: string) => void,
    dependencies?: () => CbsHistoryDependencies | undefined,
): () => void {
    const runtime = getPersistentDataRuntime()
    let unsubscribeSource: (() => void) | undefined
    let previous: ConversationViewportSnapshot | undefined
    let identity: string | undefined
    const update = () => {
        const source = runtime.getActiveConversationViewportSource()
        const snapshot = source?.snapshot()
        const nextIdentity = captureLiveDisplayParserSelection()
        const reads = dependencies?.()
        if (reads && previous && snapshot && identity === nextIdentity && previous.sourceToken === snapshot.sourceToken
            && (previous.version === snapshot.version || previous.historyVersion !== snapshot.historyVersion)
            && !hasCbsHistoryChanges(snapshot, previous.historyVersion, reads)) {
            previous = snapshot
            return
        }
        identity = nextIdentity
        previous = snapshot
        listener(nextIdentity, JSON.stringify([snapshot?.sourceToken, snapshot?.version, snapshot?.historyVersion]))
    }
    const bindSource = () => {
        unsubscribeSource?.()
        unsubscribeSource = runtime.getActiveConversationViewportSource()?.subscribe(update)
        update()
    }
    const unsubscribe = runtime.subscribeActiveConversationViewportSource(bindSource)
    bindSource()
    return () => { unsubscribe(); unsubscribeSource?.() }
}
