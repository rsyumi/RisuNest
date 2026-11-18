import type { Chat, character } from './database.svelte'
import type { ConversationMutation, PersistentUnitMutation } from './persistentDataStore'
import { canonicalJson, canonicalClone, messageReplaceRange, requiresWholeObjectCapture } from './saveCoordinatorHelpers'
import { isConversationSummaryStub } from './conversationResidency'

type CompleteCharacter = character

export function diffFields(
    components: string[], beforeValue: object, afterValue: object,
    excluded: ReadonlySet<string> = new Set(),
): PersistentUnitMutation[] {
    const before = beforeValue as Record<string, unknown>, after = afterValue as Record<string, unknown>
    const mutations: PersistentUnitMutation[] = []
    for (const field of new Set([...Object.keys(before), ...Object.keys(after)])) {
        if (excluded.has(field) || before[field] === after[field]) continue
        if (canonicalJson({ value: before[field] }) === canonicalJson({ value: after[field] })) continue
        const key = JSON.stringify([...components, field])
        mutations.push(!Object.hasOwn(after, field) || after[field] === undefined
            ? { key, type: 'delete' } : { key, type: 'set', value: canonicalClone(after[field]) })
    }
    return mutations
}

export function captureMaterializedCharacter(value: CompleteCharacter): CompleteCharacter {
    if (requiresWholeObjectCapture(value)) return canonicalClone(value)
    const result = Object.fromEntries(Object.keys(value).filter((key) => key !== 'chats' && value[key] !== undefined)
        .map((key) => [key, canonicalClone(value[key])])) as CompleteCharacter
    result.chats = value.chats.map((chat) => requiresWholeObjectCapture(chat,
        isConversationSummaryStub(chat) || !Object.prototype.propertyIsEnumerable.call(chat, 'message') ? new Set(['message']) : undefined)
        ? canonicalClone(chat) : Object.fromEntries(Object.keys(chat)
        .filter((key) => chat[key] !== undefined && (key !== 'message' || (!isConversationSummaryStub(chat)
            && Object.prototype.propertyIsEnumerable.call(chat, 'message'))))
        .map((key) => [key, key === 'message'
            ? chat.message.map((message, index) => JSON.parse(canonicalJson({ [index]: message }))[index])
            : canonicalClone(chat[key])])) as Chat)
    return result
}

export function diffMaterializedCharacter(before: CompleteCharacter, after: CompleteCharacter): {
    unitMutations: PersistentUnitMutation[]; conversations: ConversationMutation[]
} {
    const unitMutations = diffFields(['character', after.chaId], before, after,
        new Set(['chats', 'chaId', 'chatFolders']))

    const conversations: ConversationMutation[] = []
    const previous = new Map(before.chats.map((chat) => [chat.id, chat]))
    for (const [index, chat] of after.chats.entries()) {
        if (!chat.id) throw new TypeError('Conversation requires a stable ID')
        const old = previous.get(chat.id)
        if (!old) {
            if (!Object.hasOwn(chat, 'message')) throw new TypeError('New conversation requires messages')
            const { message, ...conversation } = chat
            conversations.push({ type: 'replace-range', characterId: after.chaId, conversationId: chat.id,
                start: 0, deleteCount: 0, messages: message, conversation, configuredIndex: index })
            continue
        }
        unitMutations.push(...diffFields(['conversation', after.chaId, chat.id], old, chat, new Set(['message', 'id'])))
        if (chat.message !== old.message && Object.hasOwn(chat, 'message') && Object.hasOwn(old, 'message')) {
            const range = messageReplaceRange(old.message, chat.message)
            if (range.deleteCount || range.messages.length) {
                conversations.push({ type: 'replace-range', characterId: after.chaId, conversationId: chat.id, ...range })
            }
        }
    }
    const nextIds = new Set(after.chats.map((chat) => chat.id))
    for (const old of before.chats) if (!nextIds.has(old.id)) {
        conversations.push({ type: 'delete', characterId: after.chaId, conversationId: old.id })
    }
    const orderValue = (value: CompleteCharacter) => ({ ids: value.chats.map((chat) => chat.id), folders: value.chatFolders ?? [] })
    if (canonicalJson(orderValue(before)) !== canonicalJson(orderValue(after))) {
        unitMutations.push({ key: JSON.stringify(['order', 'conversations', after.chaId]), type: 'set', value: orderValue(after) })
    }
    return { unitMutations, conversations }
}

export const recordCollections = new Map([
    ['modules', 'id'], ['plugins', 'name'], ['loadouts', 'id'], ['customModels', 'id'],
])

export function diffRecordCollection(collection: string, before: unknown[], after: unknown[]): PersistentUnitMutation[] {
    const identity = recordCollections.get(collection)
    if (!identity) throw new TypeError('Unknown record collection')
    const index = (values: unknown[]) => new Map(values.map((value) => {
        const id = (value as Record<string, unknown>)[identity]
        if (typeof id !== 'string' || !id) throw new TypeError(`${collection} requires stable IDs`)
        return [id, value] as const
    }))
    const previous = index(before), next = index(after)
    if (previous.size !== before.length || next.size !== after.length) throw new TypeError('Duplicate record ID')
    const mutations: PersistentUnitMutation[] = []
    for (const id of new Set([...previous.keys(), ...next.keys()])) {
        if (canonicalJson(previous.get(id) ?? null) === canonicalJson(next.get(id) ?? null)) continue
        if (collection !== 'plugins' && previous.has(id) !== next.has(id)) mutations.push(next.has(id)
            ? { key: JSON.stringify(['exists', collection, id]), type: 'set', value: true }
            : { key: JSON.stringify(['exists', collection, id]), type: 'delete' })
        mutations.push(next.has(id) ? { key: JSON.stringify(['record', collection, id]), type: 'set', value: next.get(id) }
            : { key: JSON.stringify(['record', collection, id]), type: 'delete' })
    }
    if (canonicalJson([...previous.keys()]) !== canonicalJson([...next.keys()])) {
        mutations.push({ key: JSON.stringify(['order', collection]), type: 'set', value: [...next.keys()] })
    }
    return mutations
}

export const CHARACTER_SHARED_FIELDS: ReadonlySet<string> = new Set(['statics', 'additionalAssets', 'additionalData', 'additionalText', 'alternateGreetings', 'backgroundCSS', 'backgroundHTML', 'bias', 'ccAssets', 'characterVersion', 'coldStoragedChats', 'coldstorage', 'creation_date', 'creator', 'creatorNotes', 'customModuleToggle', 'customscript', 'defaultVariables', 'depth_prompt', 'desc', 'doNotChangeSeperateModels', 'emotionImages', 'escapeOutput', 'exampleMessage', 'extentions', 'firstMessage', 'firstMsgIndex', 'fishSpeechConfig', 'globalLore', 'gptSoVitsConfig', 'hfTTS', 'hideChatIcon', 'image', 'imported', 'inlayViewScreen', 'largePortrait', 'license', 'loreExt', 'lorePlus', 'loreSettings', 'lowLevelAccess', 'modification_date', 'moduleNamespace', 'modules', 'naittsConfig', 'name', 'newGenData', 'nickname', 'notes', 'oaiTTSConfig', 'oaiVoice', 'onboarding', 'personality', 'postHistoryInstructions', 'prebuiltAssetCommand', 'prebuiltAssetExclude', 'prebuiltAssetStyle', 'private', 'realmId', 'removedQuotes', 'replaceGlobalNote', 'scenario', 'scriptstate', 'sdData', 'source', 'suggestMessages', 'supaMemory', 'systemPrompt', 'tags', 'translatorNote', 'trashTime', 'triggerscript', 'ttsMode', 'ttsReadOnlyQuoted', 'ttsSpeech', 'utilityBot', 'viewScreen', 'virtualscript', 'vits', 'voicevoxConfig'])
export const CONVERSATION_SHARED_FIELDS: ReadonlySet<string> = new Set(['GLGlobalVariables', 'authorNotePreset', 'bindedPersona', 'bookmarkNames', 'bookmarks', 'fmIndex', 'folderId', 'hypaV2Data', 'hypaV3Data', 'lastDate', 'lastMemory', 'localLore', 'modules', 'name', 'note', 'onboardingDone', 'promptPresetId', 'rerollRecovery', 'savedToggleValues', 'scriptstate', 'sdData', 'suggestMessages', 'supaMemoryData', 'toolCalls', 'useLocallySetGlobalVariables'])
