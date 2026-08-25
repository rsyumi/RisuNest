import type { Message } from './storage/database.svelte'

function scopedKey(scope: string, kind: 'chat' | 'legacy', value: string): string {
    return `${scope.length}:${scope}|${kind}:${value.length}:${value}`
}

export class ChatRenderIdentityRegistry {
    private legacyKeys = new WeakMap<Message, string[]>()
    private nextLegacyKey = 0

    resolve(scope: string, messages: readonly Message[]): string[] {
        const idCounts = new Map<string, number>()
        for (const message of messages) {
            if (message.chatId) {
                idCounts.set(message.chatId, (idCounts.get(message.chatId) ?? 0) + 1)
            }
        }

        const objectOccurrences = new Map<Message, number>()
        return messages.map((message) => {
            if (message.chatId && idCounts.get(message.chatId) === 1) {
                return scopedKey(scope, 'chat', message.chatId)
            }

            const occurrence = objectOccurrences.get(message) ?? 0
            objectOccurrences.set(message, occurrence + 1)
            let keys = this.legacyKeys.get(message)
            if (!keys) {
                keys = []
                this.legacyKeys.set(message, keys)
            }
            keys[occurrence] ??= String(this.nextLegacyKey++)
            return scopedKey(scope, 'legacy', keys[occurrence])
        })
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
            write(text)
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
