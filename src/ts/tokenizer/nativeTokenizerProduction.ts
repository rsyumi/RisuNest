import {
    invokeNativeTokenizerBatch,
    resolveNativeTokenizerRoute,
    type NativeTokenizerId,
    type NativeTokenizerInvoke,
} from './nativeTokenizer'

export const NATIVE_TOKENIZER_MIN_BATCH_ITEMS = 100

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

export async function tryNativeTokenizerIdsBatch(
    texts: string[],
    context: ProductionNativeTokenizerContext,
    invokeCommand?: NativeTokenizerInvoke,
): Promise<number[][] | null> {
    if (texts.length < NATIVE_TOKENIZER_MIN_BATCH_ITEMS) {
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
    const response = await invokeNativeTokenizerBatch(route, texts, 'ids', invokeCommand)
    return response.mode === 'ids' ? response.ids : null
}
