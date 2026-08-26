import {
    invokeNativeTokenizerBatch,
    resolveNativeTokenizerRoute,
    type NativeTokenizerId,
    type NativeTokenizerInvoke,
} from './nativeTokenizer'

export const NATIVE_TOKENIZER_MIN_BATCH_ITEMS = 100
export const NATIVE_TOKENIZER_MAX_BATCH_ITEMS = 1_000
export const NATIVE_TOKENIZER_MAX_AGGREGATE_INPUT_BYTES = 1_048_576

export type NativeTokenizerBatchCandidate = {
    itemCount: number
    aggregateInputBytes: () => number
    buildTexts: () => string[]
}

export type NativeTokenizerIdsBatchCandidate = NativeTokenizerBatchCandidate

export type ProductionNativeTokenizerContext = {
    isTauri: boolean
    aiModel: string
    customTokenizer: string
    modelTokenizerId: NativeTokenizerId | null
    pluginTokenizer?: string
}

function isNativeTokenizerId(tokenizerId?: string): tokenizerId is NativeTokenizerId {
    return tokenizerId === 'cl100k_base' || tokenizerId === 'o200k_base'
}

export function resolveProductionNativeTokenizerId(
    context: ProductionNativeTokenizerContext,
): NativeTokenizerId | null {
    if (!context.isTauri) {
        return null
    }
    if (context.aiModel === 'openrouter' || context.aiModel === 'reverse_proxy') {
        return context.customTokenizer === 'tik' ? 'o200k_base' : null
    }
    if (context.aiModel === 'custom') {
        return isNativeTokenizerId(context.pluginTokenizer) ? context.pluginTokenizer : null
    }
    return context.modelTokenizerId
}

async function tryNativeTokenizerBatch(
    candidate: NativeTokenizerBatchCandidate,
    context: ProductionNativeTokenizerContext,
    mode: 'count' | 'ids',
    invokeCommand?: NativeTokenizerInvoke,
): Promise<number[] | number[][] | null> {
    if (
        candidate.itemCount < NATIVE_TOKENIZER_MIN_BATCH_ITEMS ||
        candidate.itemCount > NATIVE_TOKENIZER_MAX_BATCH_ITEMS
    ) {
        return null
    }
    const tokenizerId = resolveProductionNativeTokenizerId(context)
    if (!tokenizerId) {
        return null
    }
    const route = resolveNativeTokenizerRoute(tokenizerId, context.isTauri, true)
    if (route.kind !== 'native-tiktoken') {
        return null
    }
    if (candidate.aggregateInputBytes() > NATIVE_TOKENIZER_MAX_AGGREGATE_INPUT_BYTES) {
        return null
    }
    const texts = candidate.buildTexts()
    const response = await invokeNativeTokenizerBatch(route, texts, mode, invokeCommand)
    return response.mode === 'count' ? response.counts : response.ids
}

export async function tryNativeTokenizerCountBatch(
    candidate: NativeTokenizerBatchCandidate,
    context: ProductionNativeTokenizerContext,
    invokeCommand?: NativeTokenizerInvoke,
): Promise<number[] | null> {
    return await tryNativeTokenizerBatch(candidate, context, 'count', invokeCommand) as number[] | null
}

export async function tryNativeTokenizerIdsBatch(
    candidate: NativeTokenizerIdsBatchCandidate,
    context: ProductionNativeTokenizerContext,
    invokeCommand?: NativeTokenizerInvoke,
): Promise<number[][] | null> {
    return await tryNativeTokenizerBatch(candidate, context, 'ids', invokeCommand) as number[][] | null
}
