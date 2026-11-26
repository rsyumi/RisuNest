import rfdc from 'rfdc'
import { get } from 'svelte/store'
import { DBState, selectedCharID } from '../stores.svelte'
import { risuChatParser, type CbsConditions } from '../parser/parser.svelte'
import { getChatVarFromConversation, setChatVarOnConversation } from '../parser/chatVar.svelte'
import { planChatParserHistory } from '../chatParserHistory'
import type { CbsHistoryAccess } from '../cbsHistoryAccess'
import { getRegexExecutionPlan } from '../process/regexExecutionPlan'
import { createLiveChatParserIndirections } from '../liveDisplayParserLease'
import { captureResidentCbsHistory, prepareCbsHistory } from '../prepareCbsHistory'
import { getModuleLorebooks, getModuleRegexScripts, getModules } from '../process/modules'
import type { ConversationWindowQuery, PersistentRevisionReader } from '../storage/persistentDataStore'
import { findActiveHistoryWindow } from '../process/historyWindowIndex'
import { writeHistoryWindowMessage } from '../process/historyWindowWrite'
import { setConversationChatVariable } from '../process/selectedChatVariables'
import { requestScriptDisplayRefresh } from '../process/scriptDisplayRefresh'
import { getPersonaPrompt, getUserName } from '../util'
import { cloneConversationMetadata, isMetadataOnlySelectedConversation } from '../storage/selectedConversationLifecycle'
import { assertPersistentMutationAllowed, getPersistentStorageAuthorityEpoch, getPersistentDataRuntime, peekActiveConversationSession } from '../storage/persistentDataRuntime.svelte'
import { RevisionConflictError } from '../storage/persistentDataStore'
import type { Chat, Message } from '../storage/database.svelte'

const clone = rfdc()
const variablePhases = new Map<string, Promise<void>>()

export interface ParseRisuChatOptions {
    messageIndex?: number
    role?: string
    processRegex?: boolean
    runVar?: boolean
    rmVar?: boolean
    tokenizeAccurate?: boolean
    cbsConditions?: CbsConditions
}

export class PluginParserTargetChangedError extends Error {
    constructor() {
        super('The parseRisuChat conversation is no longer available')
        this.name = 'PluginParserTargetChangedError'
    }
}

export async function parseRisuChat(text: string, options?: ParseRisuChatOptions, signal?: AbortSignal): Promise<string> {
    signal?.throwIfAborted()
    // Capture before the first await. A navigation never retargets this invocation.
    const sourceDatabase = DBState.db
    const sourceCharacter = sourceDatabase.characters[get(selectedCharID)]
    if (!sourceCharacter) throw new Error('No character selected')
    const sourceChat = sourceCharacter.chats?.[sourceCharacter.chatPage]
    if (!sourceChat) throw new Error('No active chat found')
    const epoch = getPersistentStorageAuthorityEpoch()
    const characterId = sourceCharacter.chaId
    const conversationId = sourceChat.id
    const chat = { ...cloneConversationMetadata(sourceChat), message: [] } as Chat
    const character = clone({ ...sourceCharacter, chats: [chat], chatPage: 0 })
    const database = clone({ ...sourceDatabase, characters: [character] })
    database.characters[0] = character
    const invocationChat = character.chats[0]
    const personaPrompt = getPersonaPrompt()
    const userName = getUserName()
    const modules = clone(getModules())
    const moduleLorebooks = clone(getModuleLorebooks())
    const moduleRegexScripts = clone(getModuleRegexScripts())
    const regexPlan = options?.processRegex ? getRegexExecutionPlan([
        ...(database.presetRegex ?? []), ...(character.customscript ?? []), ...moduleRegexScripts,
    ], 'editprocess') : undefined
    const parserIndirections = createLiveChatParserIndirections(database, character, invocationChat, personaPrompt)
    const planFor = (source: string) => {
        const plan = planChatParserHistory({ source, indirections: parserIndirections,
            unsafeDependencies: regexPlan?.entries.some((entry) => entry.actions.includes('repeat_back') || entry.replacement.startsWith('@@repeat_back')) ? ['repeat-back'] : [] })
        if (chatID >= 0 && regexPlan?.entries.some((entry) => entry.actions.includes('inject') || entry.replacement.startsWith('@@inject'))) {
            return { ...plan, absoluteMessageIndices: [...plan.absoluteMessageIndices, chatID] }
        }
        return plan
    }
    const chatID = options?.messageIndex ?? -1
    const windowed = isMetadataOnlySelectedConversation(sourceChat)
    const runtime = windowed ? getPersistentDataRuntime() : null
    const authority = runtime?.captureSelectedConversationAuthority()
    const viewport = runtime?.getActiveConversationViewportSource()
    const projection = authority?.overlay?.captureProjection()
    const injectSlot = chatID >= 0 && regexPlan?.entries.some(entry =>
        entry.actions.includes('inject') || entry.replacement.startsWith('@@inject')) ? projection?.row(chatID) : null
    const totalMessages = windowed
        ? authority?.totalMessages
        : sourceChat.message.length
    if (totalMessages === undefined) throw new PluginParserTargetChangedError()
    if (!Number.isInteger(chatID) || chatID < -1 || chatID >= totalMessages) {
        throw new Error(`Invalid messageIndex: ${chatID}`)
    }
    const pendingVariables = new Map<string, string>()
    const pendingMessages = new Map<number, { expected: Message; data: string }>()
    const matchesMessage = (actual: Message | undefined, expected: Message) => actual
        && actual.chatId === expected.chatId && (actual.chatId || actual.data === expected.data)
    let mutationError: unknown
    const findResidentChat = () => DBState.db.characters.find((candidate) => candidate.chaId === characterId)
            ?.chats?.find((candidate) => conversationId ? candidate.id === conversationId : candidate === sourceChat)
    const residentChat = () => {
        assertPersistentMutationAllowed(epoch)
        return findResidentChat()
    }
    const setVariable = (key: string, value: string) => {
        try {
            signal?.throwIfAborted()
            const current = residentChat()
            const session = peekActiveConversationSession()
            const selected = DBState.db.characters[get(selectedCharID)]
            if (current && !isMetadataOnlySelectedConversation(current)
                && (session?.matchesConversation(characterId, current)
                    || (selected?.chaId === characterId && selected.chats[selected.chatPage] === current))) {
                setConversationChatVariable(characterId, current, key, value)
            } else pendingVariables.set(key, value)
            setChatVarOnConversation(invocationChat, key, value)
        } catch (error) {
            mutationError = error
            throw error
        }
    }
    async function uniqueStoredIndex(reader: Pick<PersistentRevisionReader, 'readConversationWindow'>, id: string) {
        const occurrence = async (anchorOccurrence: 'first' | 'last') => {
            const result = await reader.readConversationWindow({ characterId, conversationId, anchorMessageId: id, anchorOccurrence, before: 0, after: 0 })
            return result?.value.messages[0]?.chatId === id ? result.value.startIndex : null
        }
        const first = await occurrence('first')
        return first !== null && await occurrence('last') === first ? first : null
    }
    async function commitPending() {
        if (!pendingVariables.size && !pendingMessages.size) return
        assertPersistentMutationAllowed(epoch)
        const variables = new Map(pendingVariables)
        const messages = new Map(pendingMessages)
        pendingVariables.clear()
        pendingMessages.clear()
        await getPersistentDataRuntime().commitPreparedUnitIntent('plugin-parse-risu-chat', async (reader) => {
            assertPersistentMutationAllowed(epoch)
            const metadata = await reader.readConversationMetadata(characterId, conversationId)
            if (!metadata) throw new PluginParserTargetChangedError()
            const scriptstate = { ...metadata.value.conversation.scriptstate }
            for (const [key, value] of variables) scriptstate['$' + key] = value
            const conversations = []
            for (const [index, { expected, data }] of messages) {
                let storedIndex = injectSlot && 'storedIndex' in injectSlot ? injectSlot.storedIndex : index
                if (authority && reader.revision !== authority.storeRevision) {
                    const original = await leasePromise
                    if (!original || !expected.chatId || await uniqueStoredIndex(original, expected.chatId) !== storedIndex)
                        throw new PluginParserTargetChangedError()
                    const relocated = await uniqueStoredIndex(reader, expected.chatId)
                    if (relocated === null) throw new PluginParserTargetChangedError()
                    storedIndex = relocated
                }
                const result = await reader.readConversationWindow({ characterId, conversationId, startIndex: storedIndex, limit: 1 })
                const message = result?.value.messages[0]
                if (!matchesMessage(message, expected)) {
                    throw new PluginParserTargetChangedError()
                }
                conversations.push({ type: 'replace-range' as const, characterId, conversationId,
                    start: storedIndex, deleteCount: 1, messages: [{ ...message, data }] })
            }
            return {
                unitMutations: variables.size ? [{ type: 'set' as const,
                    key: JSON.stringify(['conversation', characterId, conversationId, 'scriptstate']), value: scriptstate }] : [],
                conversations,
            }
        })
        const selected = DBState.db.characters[get(selectedCharID)]
        if (selected?.chaId === characterId && selected.chats[selected.chatPage]?.id === conversationId) {
            requestScriptDisplayRefresh({ keys: [...variables.keys()] })
        }
    }
    const initialPlan = planFor(text ?? '')
    const residentMessages = !windowed && options?.processRegex ? clone(sourceChat.message) : undefined
    const initialResidentHistory = !windowed ? captureResidentCbsHistory(residentMessages ?? sourceChat.message, initialPlan, chatID) : undefined
    const resident = new Map<number, Message>()
    const needsRows = options?.processRegex || initialPlan.requiresFullHistory || initialPlan.absoluteMessageIndices.length > 0
        || initialPlan.requirements.some((requirement) => requirement !== 'count')
    if (windowed && needsRows) {
        for (const row of viewport?.captureResidentRows?.() ?? []) resident.set(row.absoluteIndex, clone(row.message))
    }
    let historyAccess: CbsHistoryAccess
    const leasePromise = windowed && needsRows ? runtime!.store.acquireRevision(authority!.storeRevision) : undefined
    const storedReader = {
        readConversationWindow: async (query: ConversationWindowQuery) => (await leasePromise)!.readConversationWindow(query),
        readConversationMessageMetadataWindow: async (query: ConversationWindowQuery) => (await leasePromise)!.readConversationMessageMetadataWindow!(query),
    }
    const windowReader = viewport?.captureProjectedReader?.(storedReader) ?? storedReader
    // Keep a rejected acquisition observed while a preceding variable commit finishes.
    void leasePromise?.catch(() => {})
    const prepareHistory = async (source: string, index: number) => {
        const plan = planFor(source)
        if (!windowed) return captureResidentCbsHistory(residentMessages ?? sourceChat.message, plan, index)
        try {
            const lease = await leasePromise
            const access = await prepareCbsHistory({ characterId, conversationId, totalMessages, chatID: index, plan, signal,
                reader: {
                    revision: authority!.storeRevision,
                    readResidentMessage: (row) => resident.get(row),
                    readRoleWindow: lease?.messageMetadataReadMode === 'projected' && authority!.sessionVersion === authority!.persistedSessionVersion
                        ? (query) => windowReader.readConversationMessageMetadataWindow!(query) : undefined,
                    readConversationWindow: async (query) => {
                        const result = await windowReader.readConversationWindow(query)
                        return result ? { ...result, value: { ...result.value, totalMessages } } : null
                    },
                },
            })
            for (const [row, message] of access.preparedRows()) resident.set(row, message)
            historyAccess = access
            return access
        } catch (error) {
            if (error instanceof RevisionConflictError) throw new PluginParserTargetChangedError()
            throw error
        }
    }
    // Only the first CBS pass can write variables. Release its ordering boundary
    // before invoking scripts, since a plugin hook may call this API again.
    const variableKey = JSON.stringify([epoch, characterId, conversationId])
    const precedingVariables = variablePhases.get(variableKey)
    let releaseVariables: (() => void) | undefined
    if (windowed || precedingVariables) {
        const ownPhase = new Promise<void>((resolve) => { releaseVariables = resolve })
        const phase = precedingVariables ? precedingVariables.then(() => ownPhase) : ownPhase
        variablePhases.set(variableKey, phase)
        void phase.then(() => {
            if (variablePhases.get(variableKey) === phase) variablePhases.delete(variableKey)
        })
    }
    const localTarget = injectSlot && 'message' in injectSlot ? authority!.overlay!.bind(chatID, injectSlot.message) : null
    let waitingForVariables = !!precedingVariables
    let failed = false
    try {
        if (precedingVariables) {
            if (signal) {
                signal.throwIfAborted()
                let onAbort!: () => void
                try {
                    await new Promise<void>((resolve, reject) => {
                        onAbort = () => reject(signal.reason)
                        signal.addEventListener('abort', onAbort, { once: true })
                        void precedingVariables.then(resolve, reject)
                    })
                } finally { signal.removeEventListener('abort', onAbort) }
            } else await precedingVariables
            waitingForVariables = false
            signal?.throwIfAborted()
            if (getPersistentStorageAuthorityEpoch() !== epoch) throw new PluginParserTargetChangedError()
            const current = findResidentChat()
            const selected = DBState.db.characters[get(selectedCharID)]
            if (current && selected?.chaId === characterId && selected.chats[selected.chatPage] === current) invocationChat.scriptstate = clone(current.scriptstate ?? {})
            else {
                const metadata = await getPersistentDataRuntime().store.readConversationMetadata(characterId, conversationId)
                if (!metadata) throw new PluginParserTargetChangedError()
                invocationChat.scriptstate = clone(metadata.value.conversation.scriptstate ?? {})
            }
        }
        historyAccess = initialResidentHistory ?? await prepareHistory(text ?? '', chatID)
        if (initialPlan.requiresFullHistory) invocationChat.message = historyAccess.all() as Message[]
        const role = options?.role
        const globalChatVariables = { ...database.globalChatVariables }
        for (const [key, value] of Object.entries(invocationChat.GLGlobalVariables ?? {})) {
            if (value && value !== 'null') globalChatVariables[key] = value
        }
        const cbsConditions = { ...(role ? { chatRole: role } : {}), ...(options?.cbsConditions ?? {}) }
        const parser = {
            db: database, chara: character, chatID, role, cbsConditions,
            runVar: options?.runVar, rmVar: options?.rmVar, tokenizeAccurate: options?.tokenizeAccurate,
            historyAccess, selectedCharID: 0, selectedCharacterId: characterId,
            userName, personaPrompt, modules, moduleLorebooks,
            globalChatVariables,
            getChatVar: (key: string) => getChatVarFromConversation(database, characterId, invocationChat, key),
            setChatVar: setVariable,
        }
        try {
            signal?.throwIfAborted()
            const parsed = risuChatParser(text ?? '', { ...parser })
            historyAccess.assertComplete()
            if (mutationError) throw mutationError
            if (!options?.processRegex) return parsed
            await commitPending()
            releaseVariables?.()
            releaseVariables = undefined
            const { processScriptFull } = await import('../process/scripts')
            const result = await processScriptFull(character, parsed, 'editprocess', chatID, cbsConditions, {
                signal,
                cache: 'bypass',
                invocationContext: {
                    database, character, chat: invocationChat, parser, moduleRegexScripts, prepareHistory,
                    inject(index, data) {
                        try {
                            signal?.throwIfAborted()
                            const expected = historyAccess.row(index)
                            if (!expected) throw new PluginParserTargetChangedError()
                            const current = residentChat()
                            const session = peekActiveConversationSession()
                            const selected = DBState.db.characters[get(selectedCharID)]
                            const window = findActiveHistoryWindow(characterId, conversationId)
                            if (localTarget && (!current || runtime?.captureSelectedConversationAuthority()?.overlay !== authority?.overlay))
                                throw new PluginParserTargetChangedError()
                            if (!windowed && current && session?.matchesConversation(characterId, current)) {
                                const locator = session.locate(index)
                                const message = session.readMessage(locator)
                                if (!matchesMessage(message, expected)) throw new PluginParserTargetChangedError()
                                session.edit(locator, { ...message, data })
                            } else if (current && localTarget && authority?.overlay) {
                                const absolute = authority.overlay.locate(localTarget)
                                const before = authority.overlay.readTarget(localTarget)
                                const target = runtime?.captureSelectedConversationTarget()
                                if (absolute === null || !before || !target || runtime?.captureSelectedConversationAuthority()?.overlay !== authority.overlay)
                                    throw new PluginParserTargetChangedError()
                                if (window?.isCurrent() && absolute >= window.absoluteStartIndex && absolute < window.absoluteStartIndex + window.chat.message.length) {
                                    if (!writeHistoryWindowMessage(window, absolute, () => ({ ...before, data }))) throw new PluginParserTargetChangedError()
                                } else {
                                    const controller = runtime.captureWindowedConversationMutationController(target,
                                        { ...cloneConversationMetadata(current), message: [] } as Chat, authority.overlay.count)
                                    try {
                                        if (!controller?.applyAbsoluteRanges?.([{ start: absolute, before: [before], messages: [{ ...before, data }] }]))
                                            throw new PluginParserTargetChangedError()
                                    } finally { controller?.release() }
                                }
                            } else if (current && window?.isCurrent() && projection?.epoch === authority?.overlay?.epoch &&
                                index >= window.absoluteStartIndex && index < window.absoluteStartIndex + window.chat.message.length) {
                                if (!matchesMessage(window.chat.message[index - window.absoluteStartIndex], expected)) throw new PluginParserTargetChangedError()
                                if (!writeHistoryWindowMessage(window, index, message => ({ ...message, data }))) throw new PluginParserTargetChangedError()
                            } else if (!windowed && current && !isMetadataOnlySelectedConversation(current)
                                && selected?.chaId === characterId && selected.chats[selected.chatPage] === current) {
                                if (!matchesMessage(current.message[index], expected)) throw new PluginParserTargetChangedError()
                                current.message[index].data = data
                            } else pendingMessages.set(index, { expected: pendingMessages.get(index)?.expected ?? clone(expected), data })
                            expected.data = data
                        } catch (error) {
                            mutationError = error
                            throw error
                        }
                    },
                },
            })
            if (mutationError) throw mutationError
            return result.data
        } finally { await commitPending() }
    } catch (error) {
        failed = true
        throw error
    } finally {
        releaseVariables?.()
        if (localTarget) authority!.overlay!.release(localTarget)
        if (leasePromise) {
            const cleanup = leasePromise.then((lease) => lease.release())
            // An aborted waiter must not wait for acquisition or release.
            if (waitingForVariables) void cleanup.catch(() => {})
            else {
                try { await cleanup } catch (error) { if (!failed) throw error }
            }
        }
    }
}
