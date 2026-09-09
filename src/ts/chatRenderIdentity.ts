import type { Message } from './storage/database.svelte'

function scopedKey(scope: string, kind: 'chat' | 'legacy', value: string): string {
    return `${scope.length}:${scope}|${kind}:${value.length}:${value}`
}

export class ChatRenderIdentitySequence {
    constructor(private readonly keys: readonly string[]) {}

    keyAt(index: number): string | undefined {
        return this.keys[index]
    }

    keysAt(indices: readonly number[]): string[] {
        return indices.map((index) => {
            const key = this.keyAt(index)
            if (key === undefined) throw new RangeError(`Message index ${index} is out of range`)
            return key
        })
    }

    toArray(): string[] {
        return [...this.keys]
    }
}

export class ChatRenderIdentityRegistry {
    private legacyKeys = new WeakMap<Message, string[]>()
    private issuedChatIdentities = new WeakMap<Message, Map<string, number>>()
    private nextLegacyKey = 0
    private nextChatIdentity = 0
    private registeredScope: string | null = null
    private registeredLength = 0
    private registeredKeys: string[] = []
    private registeredIdCounts = new Map<string, number>()
    private registeredObjectOccurrences = new Map<Message, number>()

    register(scope: string, messages: readonly Message[]): ChatRenderIdentitySequence {
        return this.rebuild(scope, messages)
    }

    registerAppend(
        scope: string,
        messages: readonly Message[],
        previousLength: number,
    ): ChatRenderIdentitySequence {
        if (
            scope !== this.registeredScope
            || previousLength !== this.registeredLength
            || messages.length < previousLength
        ) {
            throw new Error('Append registration does not match the registered identity sequence')
        }

        if (messages.length === previousLength) {
            return new ChatRenderIdentitySequence(this.registeredKeys)
        }

        const suffixIds: Array<string | undefined> = []
        const suffixIdCounts = new Map<string, number>()
        for (let index = previousLength; index < messages.length; index++) {
            const chatId = messages[index].chatId
            suffixIds.push(chatId)
            if (chatId) suffixIdCounts.set(chatId, (suffixIdCounts.get(chatId) ?? 0) + 1)
        }
        const changesExistingIdentity = [...suffixIdCounts].some(([chatId, count]) => (
            count > 1 || this.registeredIdCounts.has(chatId)
        ))
        if (!changesExistingIdentity) {
            const nextKeys = [...this.registeredKeys]
            const objectOccurrences = new Map(this.registeredObjectOccurrences)
            for (let offset = 0; offset < suffixIds.length; offset++) {
                const message = messages[previousLength + offset]
                const chatId = suffixIds[offset]
                const occurrence = objectOccurrences.get(message) ?? 0
                const existingLegacyKey = this.existingLegacyKey(scope, message, occurrence)
                if (chatId) this.registeredIdCounts.set(chatId, 1)
                if (
                    chatId
                    && (
                        this.issuedChatIdentity(scope, chatId, message) !== undefined
                        || existingLegacyKey === undefined
                    )
                ) {
                    this.recordIssuedChatIdentity(scope, chatId, message)
                    nextKeys.push(scopedKey(scope, 'chat', chatId))
                    continue
                }

                objectOccurrences.set(message, occurrence + 1)
                nextKeys.push(existingLegacyKey ?? this.legacyKey(scope, message, occurrence))
            }
            this.setRegistration(scope, messages.length, nextKeys, objectOccurrences)
            return new ChatRenderIdentitySequence(nextKeys)
        }

        return this.rebuild(scope, messages)
    }

    private rebuild(scope: string, messages: readonly Message[]): ChatRenderIdentitySequence {
        const messageIds = messages.map((message) => message.chatId)
        const idCounts = new Map<string, number>()
        for (const chatId of messageIds) {
            if (chatId) idCounts.set(chatId, (idCounts.get(chatId) ?? 0) + 1)
        }

        const objectOccurrences = new Map<Message, number>()
        const preferredChatIdentity = new Map<string, { index: number, issue: number }>()
        for (let index = 0; index < messages.length; index++) {
            const chatId = messageIds[index]
            if (!chatId || idCounts.get(chatId) === 1) continue
            const issue = this.issuedChatIdentity(scope, chatId, messages[index])
            const preferred = preferredChatIdentity.get(chatId)
            if (issue !== undefined && (preferred === undefined || issue < preferred.issue)) {
                preferredChatIdentity.set(chatId, { index, issue })
            }
        }
        const keys = messages.map((message, index) => {
            const chatId = messageIds[index]
            const occurrence = objectOccurrences.get(message) ?? 0
            const existingLegacyKey = this.existingLegacyKey(scope, message, occurrence)
            const issuedChatIdentity = chatId
                ? this.issuedChatIdentity(scope, chatId, message)
                : undefined
            const canUseChatIdentity = (
                chatId
                && (idCounts.get(chatId) === 1 || preferredChatIdentity.get(chatId)?.index === index)
            )
            if (
                chatId
                && canUseChatIdentity
                && (issuedChatIdentity !== undefined || existingLegacyKey === undefined)
            ) {
                this.recordIssuedChatIdentity(scope, chatId, message)
                return scopedKey(scope, 'chat', chatId)
            }

            objectOccurrences.set(message, occurrence + 1)
            return existingLegacyKey ?? this.legacyKey(scope, message, occurrence)
        })
        this.registeredIdCounts = idCounts
        this.setRegistration(scope, messages.length, keys, objectOccurrences)
        return new ChatRenderIdentitySequence(keys)
    }

    private issuedChatIdentity(scope: string, chatId: string, message: Message): number | undefined {
        return this.issuedChatIdentities.get(message)?.get(scopedKey(scope, 'chat', chatId))
    }

    private recordIssuedChatIdentity(scope: string, chatId: string, message: Message): void {
        let identities = this.issuedChatIdentities.get(message)
        if (!identities) {
            identities = new Map()
            this.issuedChatIdentities.set(message, identities)
        }
        const key = scopedKey(scope, 'chat', chatId)
        if (!identities.has(key)) identities.set(key, this.nextChatIdentity++)
    }

    private existingLegacyKey(scope: string, message: Message, occurrence: number): string | undefined {
        const key = this.legacyKeys.get(message)?.[occurrence]
        return key === undefined ? undefined : scopedKey(scope, 'legacy', key)
    }

    private legacyKey(scope: string, message: Message, occurrence: number): string {
        let keys = this.legacyKeys.get(message)
        if (!keys) {
            keys = []
            this.legacyKeys.set(message, keys)
        }
        keys[occurrence] ??= String(this.nextLegacyKey++)
        return scopedKey(scope, 'legacy', keys[occurrence])
    }

    private setRegistration(
        scope: string,
        messageCount: number,
        keys: string[],
        objectOccurrences: Map<Message, number>,
    ): void {
        this.registeredScope = scope
        this.registeredLength = messageCount
        this.registeredKeys = keys
        this.registeredObjectOccurrences = objectOccurrences
    }

    resolve(scope: string, messages: readonly Message[]): string[] {
        return this.register(scope, messages).toArray()
    }

    clearRegistration(): void {
        this.registeredScope = null
        this.registeredLength = 0
        this.registeredKeys = []
        this.registeredIdCounts.clear()
        this.registeredObjectOccurrences.clear()
    }
}

export interface ChatParserCharacterDependencies {
    chaId: string
    virtualscript?: string
    customscript?: readonly unknown[]
    additionalAssets?: readonly unknown[]
    emotionImages?: readonly unknown[]
    triggerscript?: readonly unknown[]
}

export interface ChatRenderSignatureInput {
    message: Message
    index: number
    totalLength: number
    largePortrait: boolean
    reloadPointer: number
    globalReloadPointer: number
    activeStreamingMessage: boolean
    bookmarked?: boolean
    resolvedImage: string | null
    displayName: string
    parserCharacter: ChatParserCharacterDependencies | null
    parserCharacterStamp: string | null
}

export interface ChatRenderSignature {
    content: string | null
    role: Message['role']
    isComment: boolean
    disabled: Message['disabled']
    bookmarked: boolean
    generationModel: string | null
    generationId: string | null
    inputTokens: number | null
    outputTokens: number | null
    maxContext: number | null
    stage1: number | null
    stage2: number | null
    stage3: number | null
    stage4: number | null
    index: number
    liveTailLengthRevision: number
    largePortrait: boolean
    reloadPointer: number
    globalReloadPointer: number
    resolvedImage: string | null
    displayName: string
    parserCharacter: ChatParserCharacterDependencies | null
    parserCharacterStamp: string | null
}

const PARSER_STRING_CACHE_MIN_LENGTH = 256
const PARSER_STRING_CACHE_MAX_ENTRIES = 256
const PARSER_STRING_CACHE_MAX_CODE_UNITS = 2 * 1024 * 1024
const parserStringStampCache = new Map<string, string>()
let parserStringStampCacheCodeUnits = 0

function largeParserStringStamp(value: string): string {
    const cached = parserStringStampCache.get(value)
    if (cached !== undefined) {
        parserStringStampCache.delete(value)
        parserStringStampCache.set(value, cached)
        return cached
    }

    let first = 0x811c9dc5
    let second = 0x9e3779b9
    for (let index = 0; index < value.length; index++) {
        const code = value.charCodeAt(index)
        first = Math.imul(first ^ code, 0x01000193)
        second = Math.imul(second ^ code, 0x85ebca6b)
    }
    const stamp = `${(first >>> 0).toString(16).padStart(8, '0')}${(second >>> 0).toString(16).padStart(8, '0')}`
    if (value.length <= PARSER_STRING_CACHE_MAX_CODE_UNITS) {
        while (
            parserStringStampCache.size >= PARSER_STRING_CACHE_MAX_ENTRIES ||
            parserStringStampCacheCodeUnits + value.length >
                PARSER_STRING_CACHE_MAX_CODE_UNITS
        ) {
            const oldest = parserStringStampCache.keys().next().value!
            parserStringStampCache.delete(oldest)
            parserStringStampCacheCodeUnits -= oldest.length
        }
        parserStringStampCache.set(value, stamp)
        parserStringStampCacheCodeUnits += value.length
    }
    return stamp
}

export function createChatParserDependencyStamp(character: ChatParserCharacterDependencies | null): string | null {
    if (!character) return null

    let first = 0x811c9dc5
    let second = 0x9e3779b9
    const seen = new WeakSet<object>()
    const write = (value: string) => {
        for (let index = 0; index < value.length; index++) {
            const code = value.charCodeAt(index)
            first = Math.imul(first ^ code, 0x01000193)
            second = Math.imul(second ^ code, 0x85ebca6b)
        }
    }
    const visit = (value: unknown): void => {
        if (value === null) {
            write('null;')
            return
        }
        const valueType = typeof value
        if (valueType !== 'object') {
            const text = String(value)
            write(`${valueType}:${text.length}:`)
            // Only immutable string leaves are cached. Keep visiting reactive
            // containers and reading their fields so in-place edits invalidate
            // the Svelte derived stamp without rescanning unchanged Lua/regex text.
            if (valueType === 'string' && text.length >= PARSER_STRING_CACHE_MIN_LENGTH) {
                write('hashed:')
                write(largeParserStringStamp(text))
            } else {
                write(text)
            }
            write(';')
            return
        }
        if (seen.has(value as object)) {
            write('cycle;')
            return
        }
        seen.add(value as object)
        if (Array.isArray(value)) {
            write(`array:${value.length};`)
            for (const item of value) visit(item)
            return
        }
        const keys = Object.keys(value as Record<string, unknown>).sort()
        write(`object:${keys.length};`)
        for (const key of keys) {
            write(`key:${key.length}:${key};`)
            visit((value as Record<string, unknown>)[key])
        }
    }

    visit(character.customscript)
    visit(character.additionalAssets)
    visit(character.emotionImages)
    visit(character.triggerscript)
    return `${(first >>> 0).toString(16).padStart(8, '0')}${(second >>> 0).toString(16).padStart(8, '0')}`
}

export function createChatRenderSignature(input: ChatRenderSignatureInput): ChatRenderSignature {
    const generation = input.message.generationInfo
    return {
        content: input.activeStreamingMessage ? null : input.message.data,
        role: input.message.role,
        isComment: input.message.isComment ?? false,
        disabled: input.message.disabled ?? false,
        bookmarked: input.bookmarked ?? false,
        generationModel: generation?.model ?? null,
        generationId: generation?.generationId ?? null,
        inputTokens: generation?.inputTokens ?? null,
        outputTokens: generation?.outputTokens ?? null,
        maxContext: generation?.maxContext ?? null,
        stage1: generation?.stageTiming?.stage1 ?? null,
        stage2: generation?.stageTiming?.stage2 ?? null,
        stage3: generation?.stageTiming?.stage3 ?? null,
        stage4: generation?.stageTiming?.stage4 ?? null,
        index: input.index,
        liveTailLengthRevision: input.index > input.totalLength - 6 ? input.totalLength : 0,
        largePortrait: input.largePortrait,
        reloadPointer: input.reloadPointer,
        globalReloadPointer: input.globalReloadPointer,
        resolvedImage: input.resolvedImage,
        displayName: input.displayName,
        parserCharacter: input.parserCharacter,
        parserCharacterStamp: input.parserCharacterStamp,
    }
}

function sameParserCharacter(
    left: ChatParserCharacterDependencies | null,
    right: ChatParserCharacterDependencies | null,
): boolean {
    return left === right || (
        left !== null
        && right !== null
        && left.chaId === right.chaId
        && left.virtualscript === right.virtualscript
        && left.customscript === right.customscript
        && left.additionalAssets === right.additionalAssets
        && left.emotionImages === right.emotionImages
        && left.triggerscript === right.triggerscript
    )
}

export function areChatRenderSignaturesEqual(
    left: ChatRenderSignature | undefined,
    right: ChatRenderSignature,
): boolean {
    return left !== undefined
        && left.content === right.content
        && left.role === right.role
        && left.isComment === right.isComment
        && left.disabled === right.disabled
        && left.bookmarked === right.bookmarked
        && left.generationModel === right.generationModel
        && left.generationId === right.generationId
        && left.inputTokens === right.inputTokens
        && left.outputTokens === right.outputTokens
        && left.maxContext === right.maxContext
        && left.stage1 === right.stage1
        && left.stage2 === right.stage2
        && left.stage3 === right.stage3
        && left.stage4 === right.stage4
        && left.index === right.index
        && left.liveTailLengthRevision === right.liveTailLengthRevision
        && left.largePortrait === right.largePortrait
        && left.reloadPointer === right.reloadPointer
        && left.globalReloadPointer === right.globalReloadPointer
        && left.resolvedImage === right.resolvedImage
        && left.displayName === right.displayName
        && sameParserCharacter(left.parserCharacter, right.parserCharacter)
        && left.parserCharacterStamp === right.parserCharacterStamp
}

/** The caller must keep the same row and conversation owner. */
export function canRefreshChatRenderInPlace(
    left: ChatRenderSignature | undefined,
    right: ChatRenderSignature,
): boolean {
    return (
        left !== undefined &&
        (left.content === null) === (right.content === null) &&
        areChatRenderSignaturesEqual(
            {
                ...left,
                content: right.content,
                liveTailLengthRevision: right.liveTailLengthRevision,
                reloadPointer: right.reloadPointer,
                globalReloadPointer: right.globalReloadPointer,
            },
            right,
        )
    )
}
