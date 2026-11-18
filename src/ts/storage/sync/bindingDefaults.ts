type JsonRecord = Record<string, unknown>

const localRootKeys = new Set(["loreBookPage", "didFirstSetup", "nanogptSubscriptionState", "saveTime", "lastPatchNoteCheckVersion", "statics", "vertexAccessToken", "vertexAccessTokenExpires", "authRefreshes"])
const derivedRootKeys = new Set(["characters", "apiType", "proxyKey", "mainPrompt", "jailbreak", "globalNote", "temperature", "maxContext", "maxResponse", "frequencyPenalty", "PresensePenalty", "formatingOrder", "aiModel", "username", "userIcon", "userNote", "forceReplaceUrl", "plugins", "currentPluginProvider", "textgenWebUIStreamURL", "textgenWebUIBlockingURL", "subModel", "botPresets", "promptPreprocess", "bias", "localNetworkMode", "localNetworkTimeoutSec", "koboldURL", "autoSuggestPrompt", "autoSuggestPrefix", "autoSuggestClean", "account", "proxyRequestModel", "ooba", "ainconfig", "personaPrompt", "openrouterRequestModel", "personas", "NAIsettings", "colorScheme", "promptTemplate", "NAIadventure", "NAIappendName", "localStopStrings", "customProxyRequestModel", "reverseProxyOobaArgs", "translatorPrompt", "translatorMaxResponse", "top_p", "promptSettings", "top_k", "repetition_penalty", "min_p", "top_a", "modules", "instructChatTemplate", "JinjaTemplate", "openrouterProvider", "useInstructPrompt", "customPromptTemplateToggle", "globalChatVariables", "templateDefaultVariables", "moduleIntergration", "jsonSchemaEnabled", "jsonSchema", "strictJsonSchema", "extractJson", "messageNameTemplate", "namedMessageRole", "customAPIFormat", "systemContentReplacement", "systemRoleReplacement", "seperateParametersEnabled", "seperateParameters", "customFlags", "enableCustomFlags", "presetRegex", "reasoningEffort", "thinkingTokens", "thinkingType", "deepseekThinkingType", "adaptiveThinkingEffort", "deepseekReasoningEffort", "outputImageModal", "seperateModelsForAxModels", "seperateModels", "modelTools", "fallbackModels", "fallbackWhenBlankResponse", "customModels", "verbosity", "dynamicOutput", "pluginCustomStorage", "pluginStorageMeta", "loadouts"])
const recordCollections = new Set(['characters', 'botPresets', 'personas', 'modules', 'plugins', 'loadouts', 'customModels'])

function record(value: unknown): value is JsonRecord {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function canonical(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(canonical)
    if (!record(value)) return value
    return Object.fromEntries(Object.keys(value).sort().filter(key => value[key] !== undefined)
        .map(key => [key, canonical(value[key])]))
}

function libraryProjection(library: JsonRecord): unknown {
    const ids = new Map<string, string>()
    const records = new Set<JsonRecord>()
    const register = (items: unknown, scope: string) => {
        if (!Array.isArray(items)) return
        items.forEach((item, index) => {
            if (!record(item)) return
            records.add(item)
            for (const key of ['id', 'chaId']) {
                if (typeof item[key] === 'string') ids.set(item[key], `${scope}:${index}`)
            }
            register(item.chats, `${scope}:${index}:chats`)
            if (record(item.embeddedModule)) register([item.embeddedModule], `${scope}:${index}:module`)
        })
    }
    for (const key of recordCollections) register(library[key], key)
    const project = (value: unknown, scope: string, reference = false): unknown => {
        if (typeof value === 'string') return reference ? ids.get(value) ?? value : value
        if (Array.isArray(value)) return value.map(item => project(item, scope, reference))
        if (!record(value)) return value
        return Object.fromEntries(Object.entries(value).filter(([key]) => {
            if (records.has(value) && ['id', 'chaId'].includes(key)) return false
            if (scope === 'characters' && ['chatPage', 'lastInteraction', 'reloadKeys'].includes(key)) return false
            if (['isStreaming', 'activeStreamingDisplayOptimizationMode', 'configured_index'].includes(key)) return false
            return true
        }).map(([key, item]) => [key, project(item, scope,
            (records.has(value) && ['id', 'chaId'].includes(key)) ||
            ['bindedPersona', 'promptPresetId', 'characterIds', 'enabledModules', 'charOrder', 'modules', 'characters', 'folderId'].includes(key))]))
    }
    return canonical(Object.fromEntries(Object.entries(library).filter(([key]) =>
        recordCollections.has(key) || (!localRootKeys.has(key) && !derivedRootKeys.has(key) && !['botPresetsId', 'selectedPersona', 'hypaV3PresetId', 'translatorPresetId', 'explicitGlobalChatVariables', 'protectedPresetValues'].includes(key)))
        .map(([key, value]) => [key, project(value, key)])))
}

export interface BindingLocalContent {
    /** The native store sends character details only as this count. */
    library: JsonRecord
    characterCount: string
    factoryLibrary: JsonRecord
    sharedVariables?: JsonRecord
    factorySharedVariables?: JsonRecord
    opaqueSharedUnitCount: string
    protectedValues?: JsonRecord
    managedAliasCount: string
    factoryManagedAliasCount: string
    ordinaryPluginValueCount: string
    hypaValueCount: string
    pluginLocalValueCount: string
}

export function validateBindingCount(value: unknown): string {
    if (typeof value !== 'string' || !/^(0|[1-9][0-9]*)$/.test(value) ||
        value.length > 20 || (value.length === 20 && value > '18446744073709551615')) {
        throw new Error('Invalid binding content count')
    }
    return value
}

export function hasNonDefaultBindingData(content: BindingLocalContent): boolean {
    const opaque = validateBindingCount(content.opaqueSharedUnitCount)
    const ordinary = validateBindingCount(content.ordinaryPluginValueCount)
    const hypa = validateBindingCount(content.hypaValueCount)
    const pluginLocal = validateBindingCount(content.pluginLocalValueCount)
    const aliases = validateBindingCount(content.managedAliasCount)
    const factoryAliases = validateBindingCount(content.factoryManagedAliasCount)
    // A factory library has no characters, so any character is user data.
    if (validateBindingCount(content.characterCount) !== '0') return true
    if (opaque !== '0') return true
    for (const [key,value] of Object.entries(content.protectedValues ?? {})) {
        if (JSON.stringify(canonical(value)) !== JSON.stringify(canonical(content.factoryLibrary[key]))) return true
    }
    if (JSON.stringify(canonical(content.sharedVariables ?? {})) !== JSON.stringify(canonical(content.factorySharedVariables ?? {}))) return true
    if (ordinary !== '0' || hypa !== '0' || pluginLocal !== '0') return true
    if (aliases !== factoryAliases) return true
    return JSON.stringify(libraryProjection(content.library)) !== JSON.stringify(libraryProjection(content.factoryLibrary))
}
