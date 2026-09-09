import {
    classifyChatParserHistory,
    type ChatParserHistoryClassificationInput,
} from './chatParserHistory'
import { collectLiveChatParserUnsafeDependencies } from './selectedConversationLiveParserProjection'
import {
    getCurrentCharacter,
    getCurrentChat,
    getDatabase,
    type character,
    type groupChat,
    type Database,
    type Chat,
} from './storage/database.svelte'
import type { simpleCharacterArgument } from './parser/parser.svelte'
import { getPersistentDataRuntime } from './storage/persistentDataRuntime.svelte'
import type { PersistentDataRuntime } from './storage/persistentDataRuntime'
import { getModuleRegexScripts, getModuleTriggers } from './process/modules'
import { pluginV2 } from './plugins/plugins.svelte'
import { findCharacterbyId, getPersonaPrompt } from './util'
import { createChatParserDependencyStamp } from './chatRenderIdentity'

type DisplayCharacter = character | groupChat | simpleCharacterArgument | string | null
type DisplayRuntime = Pick<
    PersistentDataRuntime,
    'captureSelectedConversationTarget' | 'acquireCompleteConversation'
>
interface LiveDisplayParserInputs extends ChatParserHistoryClassificationInput {
    requiresCompleteConversation?: boolean
    triggerStamp?: string | null
}

export function createLiveChatParserIndirections(
    database: Database,
    selectedCharacter: character | groupChat,
    conversation: Chat,
    personaPrompt: string,
): Readonly<Record<string, unknown>> {
    const character = selectedCharacter.type === 'group' ? null : selectedCharacter
    let authorNote = conversation.note ?? ''
    if (!authorNote) {
        for (const item of database.promptTemplate ?? []) {
            if (item.type !== 'authornote' || !item.defaultText) continue
            authorNote = item.defaultText
            break
        }
    }
    return {
        personality: character?.personality ?? '',
        charpersona: character?.personality ?? '',
        description: character?.desc ?? '',
        chardesc: character?.desc ?? '',
        scenario: character?.scenario ?? '',
        exampledialogue: character?.exampleMessage ?? '',
        examplemessage: character?.exampleMessage ?? '',
        persona: personaPrompt,
        userpersona: personaPrompt,
        mainprompt: database.mainPrompt ?? '',
        systemprompt: database.mainPrompt ?? '',
        jb: database.jailbreak ?? '',
        jailbreak: database.jailbreak ?? '',
        globalnote: database.globalNote ?? '',
        systemnote: database.globalNote ?? '',
        ujb: database.globalNote ?? '',
        authornote: authorNote,
    }
}

export function createLiveChatParserSource(
    database: Database,
    character: Exclude<DisplayCharacter, string | null>,
    moduleRegex: ReturnType<typeof getModuleRegexScripts>,
) {
    return {
        guiHTML: database.theme === 'customHTML' ? database.guiHTML : '',
        presetRegex: database.presetRegex ?? [],
        characterRegex: character.customscript ?? [],
        moduleRegex,
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
        character.type === 'group'
            ? moduleTriggers
            : [...(character.triggerscript ?? []), ...moduleTriggers]
    return {
        // The live CBS parser resolves a group speaker from the last message even
        // when its source contains no history expression.
        requiresCompleteConversation: character.type === 'group',
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
}) {
    return async (request: {
        source: unknown
        character: DisplayCharacter
        signal: AbortSignal
    }): Promise<{ release(): void } | null> => {
        request.signal.throwIfAborted()
        const inputs = dependencies.classify(request.source, request.character)
        const classification = classifyChatParserHistory(inputs)
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
    runtime: getPersistentDataRuntime,
    classify: captureLiveDisplayParserInputs,
})

export function captureLiveDisplayParserSelection(): string {
    const target = getPersistentDataRuntime().captureSelectedConversationTarget()
    return target
        ? JSON.stringify([target.characterId, target.conversationId, target.navigationGeneration])
        : ''
}

export function subscribeLiveDisplayParserSelection(
    listener: (identity: string) => void,
): () => void {
    const runtime = getPersistentDataRuntime()
    const update = () => listener(captureLiveDisplayParserSelection())
    update()
    return runtime.subscribeActiveConversationViewportSource(update)
}
