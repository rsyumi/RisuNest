import type { RisunestJsonKind } from './risunestHostContext'

// Every shared root field, classified for the settings methods. `excluded` keys are never
// listed, read or written; the others carry their JSON kind, `|null` when null is a stored
// value, and `read-only` when they are listed and readable but not writable. A shared root
// field missing from this table is never offered.

type SettingKeyEntry =
    | 'excluded'
    | RisunestJsonKind
    | `${RisunestJsonKind}|null`
    | `read-only ${RisunestJsonKind}`

const settingKeyTable: Record<string, SettingKeyEntry> = {
    openAIKey: 'excluded', // credential
    askRemoval: 'boolean',
    jailbreakToggle: 'boolean',
    loreBookDepth: 'number|null',
    loreBookToken: 'number|null',
    cipherChat: 'boolean',
    loreBook: 'array',
    supaMemoryPrompt: 'string',
    additionalPrompt: 'string',
    descriptionPrefix: 'string',
    language: 'string',
    translator: 'string',
    zoomsize: 'number',
    customBackground: 'string',
    autoTranslate: 'boolean',
    fullScreen: 'boolean',
    playMessage: 'boolean',
    iconsize: 'number',
    theme: 'string',
    emotionPrompt: 'string',
    waifuWidth: 'number',
    waifuWidth2: 'number',
    botPresetsId: 'read-only number', // selected through the preset methods
    sdProvider: 'string',
    webUiUrl: 'excluded', // endpoint
    sdSteps: 'number',
    sdCFG: 'number',
    sdConfig: 'object',
    NAIImgUrl: 'excluded', // endpoint
    NAIApiKey: 'excluded', // credential
    NAIImgModel: 'string',
    NAII2I: 'boolean',
    NAIREF: 'boolean',
    NAIImgConfig: 'object',
    ttsAutoSpeech: 'boolean',
    instantRemove: 'boolean',
    textTheme: 'string',
    customTextTheme: 'object',
    requestRetrys: 'number|null',
    emotionPrompt2: 'string',
    useSayNothing: 'boolean',
    showUnrecommended: 'boolean',
    elevenLabKey: 'excluded', // credential
    voicevoxUrl: 'excluded', // endpoint
    useExperimental: 'boolean',
    showMemoryLimit: 'boolean',
    roundIcons: 'boolean',
    useStreaming: 'boolean',
    supaMemoryKey: 'excluded', // credential
    hypaMemoryKey: 'excluded', // credential
    voyageApiKey: 'excluded', // credential
    supaModelType: 'string',
    textScreenColor: 'string|null', // its toggle clears it with null
    textBorder: 'boolean',
    textScreenRounded: 'boolean',
    textScreenBorder: 'string|null', // its toggle clears it with null
    hordeConfig: 'excluded', // holds a credential
    novelai: 'excluded', // holds a credential
    globalscript: 'array',
    sendWithEnter: 'boolean',
    fixedChatTextarea: 'boolean',
    clickToEdit: 'boolean',
    enableBlockPartialEdit: 'boolean',
    enableDragPartialEdit: 'boolean',
    useAutoSuggestions: 'boolean',
    claudeAPIKey: 'excluded', // credential
    useChatCopy: 'boolean',
    novellistAPI: 'excluded', // credential
    useAutoTranslateInput: 'boolean',
    imageCompression: 'boolean',
    classicMaxWidth: 'boolean',
    useChatSticker: 'boolean',
    useAdditionalAssetsPreview: 'boolean',
    usePlainFetch: 'boolean',
    hypaMemory: 'boolean',
    hypav2: 'boolean',
    memoryAlgorithmType: 'string',
    openrouterKey: 'excluded', // credential
    openrouterMiddleOut: 'boolean',
    nanogptKey: 'excluded', // credential
    nanogptRequestModel: 'string',
    nanogptRequestModelName: 'string',
    nanogptProvider: 'string',
    nanogptUseSubscriptionEndpoint: 'boolean',
    openrouterFallback: 'boolean',
    selectedPersona: 'number',
    personaNote: 'boolean',
    assetWidth: 'number',
    animationSpeed: 'number',
    botSettingAtStart: 'boolean',
    hideRealm: 'boolean',
    blockedRealmCreators: 'array',
    colorSchemeName: 'string',
    customColorScheme: 'object',
    forceProxyAsOpenAI: 'boolean',
    hypaModel: 'string',
    mancerHeader: 'excluded', // credential
    emotionProcesser: 'string',
    showMenuChatList: 'boolean',
    translatorType: 'string',
    translatorInputLanguage: 'string',
    htmlTranslation: 'boolean',
    deeplOptions: 'excluded', // holds a credential
    deeplXOptions: 'excluded', // holds a credential
    autofillRequestUrl: 'boolean',
    generationSeed: 'number|null',
    newOAIHandle: 'boolean',
    gptVisionQuality: 'string',
    reverseProxyOobaMode: 'boolean',
    huggingfaceKey: 'excluded', // credential
    fishSpeechKey: 'excluded', // credential
    allowAllExtentionFiles: 'boolean',
    translatorPresets: 'array',
    translatorPresetId: 'number',
    google: 'excluded', // holds a credential
    mistralKey: 'excluded', // credential
    chainOfThought: 'boolean',
    genTime: 'number|null',
    keiServerURL: 'excluded', // endpoint
    claudeAws: 'boolean',
    removePunctuationHypa: 'boolean',
    memoryLimitThickness: 'number',
    enabledModules: 'array',
    sideMenuRerollButton: 'boolean',
    requestInfoInsideChat: 'boolean',
    additionalParams: 'excluded', // request headers
    applyAdditionalParamsToAll: 'boolean',
    heightMode: 'string',
    noWaitForTranslate: 'boolean',
    antiClaudeOverload: 'boolean',
    maxSupaChunkSize: 'number',
    ollamaURL: 'excluded', // endpoint
    ollamaModel: 'string',
    ollamaModelSource: 'string',
    ollamaInputMode: 'string',
    ollamaRequestFormat: 'number',
    ollamaApiKey: 'excluded', // credential
    ollamaModelName: 'string',
    ollamaCloudModel: 'string',
    ollamaCloudModelName: 'string',
    ollamaThinkingMode: 'string',
    autoContinueChat: 'boolean',
    autoContinueMinTokens: 'number|null',
    removeIncompleteResponse: 'boolean',
    customTokenizer: 'string',
    hanuraiTokens: 'number',
    hanuraiSplit: 'boolean',
    hanuraiEnable: 'boolean',
    textAreaSize: 'number',
    sideBarSize: 'number',
    textAreaTextSize: 'number',
    combineTranslation: 'boolean',
    dynamicAssets: 'boolean',
    dynamicAssetsEditDisplay: 'boolean',
    disableToggleBinding: 'boolean',
    defaultToggleValues: 'object',
    togglePresets: 'array',
    authorNotePresets: 'array',
    hypaAllocatedTokens: 'number',
    hypaChunkSize: 'number',
    cohereAPIKey: 'excluded', // credential
    goCharacterOnImport: 'boolean',
    dallEQuality: 'string',
    font: 'string',
    customFont: 'string',
    lineHeight: 'number',
    stabilityModel: 'string',
    stabilityKey: 'excluded', // credential
    stabllityStyle: 'string',
    legacyTranslation: 'boolean',
    comfyConfig: 'object',
    comfyUiUrl: 'excluded', // endpoint
    useLegacyGUI: 'boolean',
    claudeCachingExperimental: 'boolean',
    hideApiKey: 'boolean',
    unformatQuotes: 'boolean',
    enableDevTools: 'read-only boolean', // development switch
    falToken: 'excluded', // credential
    falModel: 'string',
    falLora: 'string',
    falLoraName: 'string',
    falLoraScale: 'number',
    customCSS: 'string',
    betaMobileGUI: 'boolean',
    customQuotes: 'boolean',
    customQuotesData: 'array',
    customGUI: 'string',
    guiHTML: 'string',
    OAIPrediction: 'string',
    vertexPrivateKey: 'excluded', // credential
    vertexClientEmail: 'excluded', // credential
    vertexRegion: 'excluded', // endpoint
    translateBeforeHTMLFormatting: 'boolean',
    autoTranslateCachedOnly: 'boolean',
    lightningRealmImport: 'boolean',
    notification: 'boolean',
    googleClaudeTokenizing: 'boolean',
    presetChain: 'string',
    legacyMediaFindings: 'boolean',
    geminiStream: 'boolean',
    assetMaxDifference: 'number|null',
    auxModelUnderModelSettings: 'boolean',
    menuSideBar: 'boolean',
    pluginV2: 'excluded', // plugin code
    showSavingIcon: 'boolean',
    banCharacterset: 'array',
    showPromptComparison: 'boolean',
    hypaV3: 'boolean',
    hypaV3Presets: 'array',
    hypaV3PresetId: 'number',
    realmDirectOpen: 'boolean',
    OaiCompAPIKeys: 'excluded', // credential
    inlayErrorResponse: 'boolean',
    risunestInlayFormat: 'string',
    risunestInlayWebpQuality: 'number',
    risunestInlayMaxDimension: 'number',
    risunestInlaySkipReencode: 'boolean',
    risunestInlayAnimationMaxFps: 'number',
    risunestInlayAnimationStillFrame: 'boolean',
    risunestGoogleSearchApiKey: 'excluded', // credential
    risunestGoogleSearchEngineId: 'excluded', // endpoint
    risunestIrisDialogue: 'array',
    bulkEnabling: 'boolean',
    showTranslationLoading: 'boolean',
    showDeprecatedTriggerV1: 'boolean',
    showDeprecatedTriggerV2: 'boolean',
    returnCSSError: 'boolean',
    checkCorruption: 'boolean',
    toggleConfirmRecommendedPreset: 'boolean',
    useExperimentalGoogleTranslator: 'boolean',
    antiServerOverloads: 'boolean',
    hypaCustomSettings: 'excluded', // holds a credential
    localActivationInGlobalLorebook: 'boolean',
    showFolderName: 'boolean',
    automaticCachePoint: 'boolean',
    claudeRetrivalCaching: 'boolean',
    playMessageOnTranslateEnd: 'boolean',
    doNotChangeSeperateModels: 'boolean',
    hotkeys: 'array',
    doNotChangeFallbackModels: 'boolean',
    igpPrompt: 'string',
    useTokenizerCaching: 'boolean',
    showMenuHypaMemoryModal: 'boolean',
    promptInfoInsideChat: 'boolean',
    promptTextInfoInsideChat: 'boolean',
    openAIFlexProcessing: 'boolean',
    claudeBatching: 'boolean',
    claude1HourCaching: 'boolean',
    rememberToolUsage: 'boolean',
    simplifiedToolUse: 'boolean',
    requestLocation: 'string',
    newImageHandlingBeta: 'boolean',
    showFirstMessagePages: 'boolean',
    streamGeminiThoughts: 'boolean',
    hubServerType: 'excluded', // endpoint
    ImagenModel: 'string',
    ImagenImageSize: 'string',
    ImagenAspectRatio: 'string',
    ImagenPersonGeneration: 'string',
    enableScrollToActiveChar: 'boolean',
    openaiCompatImage: 'excluded', // holds a credential
    wavespeedImage: 'excluded', // holds a credential
    settingsCloseButtonSize: 'number',
    promptDiffPrefs: 'object',
    enableBookmark: 'boolean',
    hideAllImages: 'boolean',
    autoScrollToNewMessage: 'boolean',
    alwaysScrollToNewMessage: 'boolean',
    newMessageButtonStyle: 'string',
    chatLoadInitialPages: 'number',
    chatLoadAdditionalPages: 'number',
    streamingDisplayOptimizationMode: 'string',
    streamingThoughtMode: 'string',
    streamingDeferDisplayProcessing: 'boolean',
    pluginDevelopMode: 'read-only boolean', // development switch
    echoMessage: 'string',
    echoDelay: 'number',
    createFolderOnBranch: 'boolean',
    hamburgerButtonBottom: 'boolean',
    enableRemoteSaving: 'boolean',
    blockquoteStyling: 'boolean',
    dynamicModelRegistry: 'boolean',
    enableRisuaiProTools: 'boolean',
    epEnabled: 'boolean',
    seperateParametersByModel: 'boolean',
    disableSeperateParameterChangeOnPresetChange: 'boolean',
    saveSignatures: 'boolean',
    keepSessionAlive: 'string',
    longPressToPopupEditor: 'boolean',
    loadoutApplyOptions: 'object',
    disableAprilFools: 'boolean',
    customSidebarItems: 'array',
    lastLoadedLoadoutName: 'string',
    moveInsteadOfCopyOnCMPConvert: 'boolean',
    skipSavingAssetsOnWebSync: 'boolean',
    risunestChatEditPopup: 'boolean',
    risunestScrollNavigation: 'boolean',
    risunestSyncStatusUi: 'boolean',
    colorScheme: 'object',
    hypaV3Settings: 'object',
}

export type RisunestOfferedSettingKey = { access: 'available' | 'read-only'; type: RisunestJsonKind; nullable: boolean }

export type RisunestSettingKeyClass = { access: 'excluded' } | RisunestOfferedSettingKey

function classify(entry: SettingKeyEntry): RisunestSettingKeyClass {
    if (entry === 'excluded') return { access: 'excluded' }
    const readOnly = entry.startsWith('read-only ')
    const kind = readOnly ? entry.slice('read-only '.length) : entry
    const nullable = kind.endsWith('|null')
    return {
        access: readOnly ? 'read-only' : 'available',
        type: (nullable ? kind.slice(0, -'|null'.length) : kind) as RisunestJsonKind,
        nullable,
    }
}

/** Each classified key in table order. Look keys up here only, never on a plain object. */
export const risunestSettingKeyClasses: ReadonlyMap<string, RisunestSettingKeyClass> = new Map(
    Object.entries(settingKeyTable).map(([key, entry]) => [key, classify(entry)]),
)

/** The available and read-only keys in table order: the keys the settings methods offer. */
export const risunestOfferedSettingKeys: ReadonlyMap<string, RisunestOfferedSettingKey> = new Map(
    [...risunestSettingKeyClasses].flatMap(([key, entry]) => entry.access === 'excluded' ? [] : [[key, entry] as const]),
)
