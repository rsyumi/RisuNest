/**
 * RisuAI Plugin API v3 Extensions: Normative TypeScript Specification
 *
 * Pure ES-module TypeScript contract definitions (`export interface` / `export type`,
 * with NO ambient global declarations). This file is the machine-readable type
 * source of truth for host implementers, compatibility libraries, conformance
 * suites, and AI agents. Because it declares no globals, it never collides with
 * or gets shadowed by any host's base `risuai.d.ts`.
 *
 * Contract summary:
 * - Plugins continue to declare `//@api 3.0`, and `getRuntimeInfo().apiVersion`
 *   remains `'3.0'`.
 * - Every host RPC method returns a `Promise` because plugins execute inside a
 *   sandboxed iframe communicating over `postMessage`.
 * - Hosts advertise implemented extension groups individually in
 *   `getRuntimeInfo().features` and storage policy in `getRuntimeInfo().storage`.
 * - See `./README.md` for full normative behavioral rules, error precedence,
 *   bridge wire format, and conformance criteria.
 */

// ============================================================================
// 1. Feature Keys, Runtime Discovery, and Storage Capabilities
// ============================================================================

/**
 * Common extension groups recommended across hosts.
 * Each group is independently adoptable and advertised individually.
 */
export type PluginCommonFeatureKey =
    | 'chats'
    | 'library'
    | 'events'
    | 'handles'
    | 'requestContext'
    | 'modelOptions'
    | 'fetchOptions';

/**
 * Optional extension groups dependent on host architecture, storage features,
 * or platform/browser capabilities.
 */
export type PluginOptionalFeatureKey =
    | 'fetchRedirect'
    | 'storageUpdate'
    | 'settings'
    | 'changes'
    | 'inlayMedia'
    | 'messageButtons'
    | 'messageSlots'
    | 'workers'
    | 'mcp'
    | 'clipboardWrite';

/**
 * Late upstream V3 additions that vary across hosts and may be advertised
 * individually once implemented.
 */
export type PluginLateV3FeatureKey =
    | 'createInlay'
    | 'parseRisuChat'
    | 'getRemovedNodes'
    | 'dbWrite';

/** Known extension and capability keys advertised in `getRuntimeInfo().features`. */
export type PluginKnownFeatureKey =
    | PluginCommonFeatureKey
    | PluginOptionalFeatureKey
    | PluginLateV3FeatureKey;

/**
 * Feature key reported in `getRuntimeInfo().features`.
 * Unknown strings MUST be ignored by plugins.
 */
export type PluginFeatureKey = PluginKnownFeatureKey | (string & {});

/** Optional diagnostic host identity metadata. Plugins MUST NOT gate capabilities on `app.id`. */
export interface PluginAppInfo {
    /** Stable host application identifier (e.g. `'risuai'`, `'risunest'`). */
    id: string;
    /** Host application version string. */
    version: string;
}

/** Advertised semantics for `risuai.pluginStorage`. */
export interface PluginStorageCapability {
    /** `'isolated'` when each plugin has its own keyspace; `'shared'` when keys are shared across plugins. */
    scope: 'isolated' | 'shared';
    /** `true` when top-level `Uint8Array` and `ArrayBuffer` values survive `setItem`/`getItem` without JSON degradation. */
    bytes: boolean;
}

/** Advertised semantics for `risuai.getLocalPluginStorage()`. */
export interface PluginLocalStorageCapability {
    /** `'isolated'` when each plugin has its own keyspace; `'shared'` when keys are shared across plugins. */
    scope: 'isolated' | 'shared';
    /** `'device'` when persisted locally on the device; `'server'` when persisted on a remote/self-hosted server. */
    location: 'device' | 'server';
    /** `true` when binary and structured-cloneable values survive `setItem`/`getItem`. */
    structured: boolean;
}

/** Advertised storage semantics returned by `getRuntimeInfo().storage`. */
export interface PluginStorageCapabilities {
    pluginStorage: PluginStorageCapability;
    localPluginStorage: PluginLocalStorageCapability;
}

/** Extended return shape of `risuai.getRuntimeInfo()`. */
export interface PluginRuntimeInfo {
    /** Unchanged V3 API version string (remains `'3.0'`). */
    apiVersion: string;
    /** Host runtime platform identifier (e.g. `'web'`, `'tauri'`, `'node'`). */
    platform: string;
    /** Host persistence backend identifier. */
    saveMethod: string;
    /** Optional diagnostic metadata for logging and support reports. */
    app?: PluginAppInfo;
    /** Independently advertised extension groups and late upstream V3 features. */
    features?: PluginFeatureKey[];
    /** Explicit host storage policy and value-type capabilities. */
    storage?: PluginStorageCapabilities;
}

// ============================================================================
// 2. Errors and Permissions
// ============================================================================

/**
 * Machine-readable error codes attached to rejected errors from extension APIs
 * and reconstructed by the guest bridge from `RESPONSE.errorCode`.
 */
export type PluginErrorCode =
    | 'NOT_FOUND'
    | 'PERMISSION_DENIED'
    | 'UNSUPPORTED'
    | 'CONFLICT'
    | 'INVALID_ARGUMENT'
    | 'ABORTED'
    | 'BRANCH_CONFLICT'
    | 'UNAVAILABLE';

/**
 * Structured error rejected by extension APIs.
 *
 * For `'ABORTED'`, the guest bridge constructs a `DOMException(message, 'AbortError')`
 * (or an `Error` named `'AbortError'` if `DOMException` is unavailable) and defines
 * `code: 'ABORTED'` as a configurable own property so both `err.code === 'ABORTED'`
 * and `err.name === 'AbortError'` hold.
 */
export interface PluginError extends Error {
    code: PluginErrorCode;
    details?: Record<string, unknown>;
}

/** All permission identifiers recognized by `requestPluginPermission`. */
export type PluginPermissionName =
    | 'fetchLogs'
    | 'db'
    | 'dbWrite'
    | 'mainDom'
    | 'replacer'
    | 'provider'
    | 'sendChat'
    | 'inlay'
    | 'mcpAccess'
    | (string & {});

// ============================================================================
// 3. Bridge Wire Descriptors & Data-Driven Handles (`handles`)
// ============================================================================

/** Additive fields on Host -> Guest `RESPONSE` messages for structured error reconstruction. */
export interface PluginRpcErrorExtension {
    /** Existing human-readable error message. */
    error?: string;
    /** Machine-readable error code reconstructed as `err.code`. */
    errorCode?: PluginErrorCode;
    /** Optional error name hint (e.g. `'AbortError'`). */
    errorName?: string;
    /** Optional structured JSON-safe metadata reconstructed as `err.details`. */
    errorDetails?: Record<string, unknown>;
}

/** Handle resource kinds carried by `PluginHandleDescriptor`. */
export type PluginHandleKind =
    | 'provider'
    | 'uiPart'
    | 'chatPanel'
    | 'mcp'
    | 'bodyIntercepter'
    | 'replacer'
    | 'chatListener'
    | 'scriptHandler'
    | 'ttsPreprocessor'
    | 'ttsPostprocessor'
    | 'channelListener'
    | 'unload'
    | 'event'
    | 'messageSlot';

/**
 * Host -> Guest wire payload assembled by the guest bridge into a `PluginHandle`.
 * Because handles are plain data rather than `REMOTE_REF` host proxies, legacy
 * plugins that ignore registration return values never leak host registry entries.
 */
export interface PluginHandleDescriptor {
    __type: 'PLUGIN_HANDLE';
    id: string;
    kind: PluginHandleKind;
    updatable?: boolean;
}

/**
 * Plain `{ id }` registration/subscription handle assembled on the guest.
 *
 * `id` is an enumerable own string property, while `dispose` (and `update` /
 * `[Symbol.asyncDispose]` when supported by the runtime) are non-enumerable.
 * As a result, `JSON.stringify(handle)`, `{ ...handle }`, `Object.keys(handle)`,
 * and `structuredClone(handle)` all produce `{ id }`.
 */
export interface PluginHandle {
    /** Stable registration identifier (enumerable own property). */
    readonly id: string;
    /**
     * Removes the registration owned by the calling plugin.
     * Repeated calls are a safe idempotent no-op.
     * When `Symbol.asyncDispose` exists in the runtime, the guest bridge also
     * attaches this function as a non-enumerable `[Symbol.asyncDispose]` property.
     */
    dispose(): Promise<void>;
}

/**
 * Registration handle supporting owner-scoped in-place updates.
 *
 * Callback arguments in `update(...)` MUST remain top-level positional arguments
 * (never nested inside plain option objects) to stay compatible with V3
 * `factory.ts` `CALLBACK_REF` serialization.
 */
export interface PluginUpdatableHandle<TArgs extends unknown[] = unknown[]> extends PluginHandle {
    update(...args: TArgs): Promise<void>;
}

// ============================================================================
// 4. Shared Primitives, Targets, Paging, and Partial-Update Helpers
// ============================================================================

/**
 * Maps an interface of known fields into a partial update patch where:
 * - Keys listed in `TRequired` cannot be deleted with `null` (`T[K] | undefined`).
 * - All other keys accept `null` to delete the optional top-level key (`T[K] | null | undefined`).
 * - Omitted keys or keys set to `undefined` are ignored (leave existing stored value unchanged).
 * - Nested `null` values inside objects/arrays are stored verbatim as data.
 *
 * IMPORTANT: Always apply this helper to a `*KnownFields` interface WITHOUT an
 * index signature, then intersect with `{ [key: string]: unknown }` so TypeScript
 * continues to type-check every known field strictly.
 */
export type PluginRecordPatch<T, TRequired extends keyof T = never> = {
    [K in TRequired]?: T[K] | undefined;
} & {
    [K in Exclude<keyof T, TRequired>]?: T[K] | null | undefined;
};

/** Primitive values allowed in chat variables (`scriptstate`). */
export type PluginChatVariableValue = string | number | boolean;

/** Message author role. */
export type PluginMessageRole = 'user' | 'char';

/** Target identifying a character by durable ID. */
export interface PluginCharacterTarget {
    characterId: string;
}

/** Target identifying a chat within a character by durable IDs. */
export interface PluginChatTarget {
    characterId: string;
    chatId: string;
}

/** Target identifying a message within a chat by durable IDs. */
export interface PluginMessageTarget {
    characterId: string;
    chatId: string;
    messageId: string;
}

/** Target identifying a module by durable ID. */
export interface PluginModuleTarget {
    moduleId: string;
}

/**
 * Target identifying a persona by session `ref`.
 * When the persona has a durable stored `id`, `ref` equals `id`.
 */
export interface PluginPersonaTarget {
    ref: string;
}

/** Owner of a lorebook entry collection. */
export type PluginLorebookOwner =
    | { kind: 'character'; characterId: string }
    | { kind: 'chat'; characterId: string; chatId: string }
    | { kind: 'module'; moduleId: string }
    | { kind: 'persona'; personaRef: string };

/** Target identifying a lorebook entry within its owner by session `ref`. */
export interface PluginLorebookEntryTarget {
    owner: PluginLorebookOwner;
    ref: string;
}

/** Field projection options for single-record `get` reads. */
export interface PluginReadFieldsOptions<TField extends string = string> {
    /** Stored field names to include in the returned record. Unknown fields are ignored. */
    fields?: TField[];
}

/** Paging options shared by `list` methods. */
export interface PluginListOptions<TField extends string = string> {
    /**
     * Zero-based starting index. Negative values count backward from the end of
     * the collection (e.g. `-10` starts 10 items before the end, clamped to `0`).
     * Defaults to `0`.
     */
    startIndex?: number;
    /**
     * Maximum number of items to return. Must be a positive safe integer.
     * Hosts may clamp to an upper bound (at least 100); defaults to 50.
     */
    pageSize?: number;
    /** Stored field names to include on each returned item. */
    fields?: TField[];
}

/** Standard paginated list response. */
export interface PluginListResult<T> {
    items: T[];
    /** Total number of items in the target collection (or active branch path) at read time. */
    total: number;
}

// ============================================================================
// 5. Shared V3 Types Referenced by Extensions
// ============================================================================

/** Minimal OpenAI-compatible chat message shape used by `runLLMModel` and replacers. */
export interface PluginOpenAIChatMessage {
    role: 'system' | 'user' | 'assistant' | 'function' | (string & {});
    content: string;
    name?: string;
    function_call?: {
        name: string;
        arguments: string;
    };
    [key: string]: unknown;
}

/** Icon format for `registerSetting` and `registerButton`. */
export type PluginIconType = 'html' | 'img' | 'none';

/** Script handler mode for `addRisuScriptHandler`. */
export type PluginScriptMode =
    | 'display'
    | 'output'
    | 'input'
    | 'process'
    | 'editdisplay'
    | 'editoutput'
    | 'editinput'
    | 'editprocess';

/** Model mode identifier used by `runLLMModel` and `PluginRequestContext.mode`. */
export type PluginModelMode =
    | 'model'
    | 'submodel'
    | 'memory'
    | 'emotion'
    | 'otherAx'
    | 'translate'
    | (string & {});

/** Shared MCP tool definition (matches V3 `registerMCP`). */
export interface PluginMCPToolDef {
    name: string;
    description: string;
    inputSchema: Record<string, unknown>;
    annotations?: Record<string, unknown>;
}

/** Text content returned from an MCP tool invocation. */
export interface PluginMCPToolCallTextContent {
    type: 'text';
    text: string;
}

/** Image or audio content returned from an MCP tool invocation. */
export interface PluginMCPToolCallBinaryContent {
    type: 'image' | 'audio';
    data: string;
    mimeType: string;
}

/** Resource content returned from an MCP tool invocation. */
export interface PluginMCPToolCallResourceContent {
    type: 'resource';
    resource: {
        uri: string;
        mimeType: string;
        text: string;
    };
}

/** Content union returned from MCP tool invocations (matches V3 `registerMCP`). */
export type PluginMCPToolCallContent =
    | PluginMCPToolCallTextContent
    | PluginMCPToolCallBinaryContent
    | PluginMCPToolCallResourceContent;

/** Minimal SafeElement handle interface returned by `chatView.getSlotElement`. */
export interface PluginSafeElementRef {
    release(): void;
    [method: string]: unknown;
}

/** Extended provider registration options (`requestContext`). */
export interface PluginProviderOptions {
    tokenizer?: string;
    tokenizerFunc?: (content: string) => number[] | Promise<number[]>;
    /**
     * Opt-in flag for the `requestContext` extension.
     * When `true`, the provider callback receives a third trailing `PluginRequestContext` argument.
     */
    requestContext?: boolean;
}

/** Target delivered to buttons registered with `location: 'message'` (`messageButtons`). */
export interface PluginMessageButtonTarget {
    characterId: string;
    chatId: string;
    messageIndex: number;
    messageId: string | null;
    role: PluginMessageRole;
}

// ============================================================================
// 6. Request Context (`requestContext`), Model Options (`modelOptions`), Fetch (`fetchOptions`)
// ============================================================================

/**
 * Provenance category of a model request:
 * - `'run'`: Main chat generation run (send, continue, reroll, including `sendChat`).
 * - `'host'`: Host's own auxiliary model calls (memory summary, emotion, translation, suggestions, etc.).
 * - `'script'`: Model calls initiated by Lua scripts or triggers.
 * - `'plugin'`: Model calls initiated by a plugin via `runLLMModel`.
 */
export type PluginRequestOrigin = 'run' | 'host' | 'script' | 'plugin';

/** Opt-in registration options for `addRisuReplacer` and `registerBodyIntercepter`. */
export interface PluginRequestContextOptions {
    /**
     * When `true`, the host passes a trailing `PluginRequestContext` argument to the callback.
     * When omitted or `false`, the host preserves the exact V3 callback arity.
     */
    requestContext?: boolean;
}

/**
 * Structured provenance metadata passed to opt-in replacers, body interceptors,
 * and custom providers.
 */
export interface PluginRequestContext {
    /** Generation run ID when `origin === 'run'`, otherwise `null`. */
    runId: string | null;
    /**
     * Effective model mode (`'model'`, `'submodel'`, `'memory'`, `'emotion'`,
     * `'otherAx'`, `'translate'`, etc.). Replaces `'v3'` on provider `arg.mode`.
     */
    mode: PluginModelMode;
    /** Subsystem that initiated the request. */
    origin: PluginRequestOrigin;
    /** Calling plugin's internal name when `origin === 'plugin'`, otherwise `null`. */
    pluginName: string | null;
    /** Caller-supplied tag from `runLLMModel({ tag })` when present, otherwise `null`. */
    tag: string | null;
    /** Target character ID of this request (never an unrelated UI selection), or `null`. */
    characterId: string | null;
    /** Target chat ID of this request (never an unrelated UI selection), or `null`. */
    chatId: string | null;
}

/** Extended options for `risuai.runLLMModel` (`modelOptions`). */
export interface PluginRunLLMModelOptions {
    messages: PluginOpenAIChatMessage[];
    mode: PluginModelMode;
    staticModel?: string;
    allowPlugins?: boolean;
    /**
     * Maximum response tokens for this call. Overrides the user's configured default
     * for the selected mode.
     */
    maxTokens?: number;
    /**
     * Sampling temperature on the standard LLM scale (e.g. `0.0` to `2.0`),
     * NOT RisuAI's `0..200` stored setting integer. Overrides the user's configured default.
     */
    temperature?: number;
    /**
     * JSON Schema object for structured output. Forwarded to providers that support
     * structured output; ignored by providers that do not.
     */
    responseSchema?: Record<string, unknown>;
    /** Cancels the request; rejects with `ABORTED` (`AbortError`) when aborted. */
    signal?: AbortSignal;
    /** Short caller label surfaced as `PluginRequestContext.tag` to interceptors and providers. */
    tag?: string;
}

/** Secret header placeholder supported in `nativeFetch` headers. */
export interface PluginSecretHeaderRef {
    secretHeader: string;
}

/** Headers input accepted by `nativeFetch` (`fetchOptions`). */
export type PluginNativeFetchHeaders =
    | Headers
    | [string, string][]
    | Record<string, string | PluginSecretHeaderRef>;

/** Body input accepted by `nativeFetch` (`fetchOptions`). */
export type PluginNativeFetchBody =
    | string
    | ArrayBuffer
    | ArrayBufferView
    | Blob
    | FormData
    | URLSearchParams
    | null;

/** Extended options for `risuai.nativeFetch` (`fetchOptions` and optional `fetchRedirect`). */
export interface PluginNativeFetchOptions {
    /**
     * Any non-forbidden HTTP method permitted by the Fetch standard
     * (e.g. `'GET'`, `'POST'`, `'PUT'`, `'PATCH'`, `'DELETE'`, `'HEAD'`, `'OPTIONS'`).
     * Fetch-forbidden methods (`'CONNECT'`, `'TRACE'`, `'TRACK'`) reject with `INVALID_ARGUMENT`.
     */
    method?: string;
    headers?: PluginNativeFetchHeaders;
    body?: PluginNativeFetchBody;
    /** Total request cancellation signal. */
    signal?: AbortSignal;
    /**
     * Inactivity timeout in milliseconds (resets while response chunks arrive).
     * For a total elapsed timeout, pass `signal: AbortSignal.timeout(ms)`.
     */
    requestTimeoutMs?: number;
    /**
     * Routing preference matching upstream `fetchNative`:
     * `'auto'` uses the host default; `'local_network'` requests direct loopback/LAN
     * delivery instead of a web proxy where supported.
     */
    networkRoute?: 'auto' | 'local_network';
    /** Set `false` to omit the request from `getFetchLogs()`. Defaults to `true`. */
    logFetch?: boolean;
    /**
     * Redirect handling mode (`fetchRedirect` optional group).
     * On a host advertising `fetchOptions` without `fetchRedirect`, passing
     * `'manual'` or `'error'` rejects with `UNSUPPORTED`.
     */
    redirect?: 'follow' | 'manual' | 'error';
}

// ============================================================================
// 7. Data Records (`chats`, `library`, and `settings`)
// ============================================================================

/** Known stored character fields (nested `chats` and `globalLore` are intentionally excluded). */
export type PluginCharacterField =
    | 'chaId'
    | 'name'
    | 'type'
    | 'image'
    | 'nickname'
    | 'desc'
    | 'personality'
    | 'scenario'
    | 'firstMessage'
    | 'alternateGreetings'
    | 'notes'
    | 'creatorNotes'
    | 'systemPrompt'
    | 'postHistoryInstructions'
    | 'defaultVariables'
    | 'translatorNote'
    | 'tags'
    | 'modules'
    | 'characters'
    | 'chatPage'
    | 'trashTime'
    | (string & {});

/** Strictly typed known fields of a character record (no index signature). */
export interface PluginCharacterKnownFields {
    /** Durable unique character identifier. */
    readonly chaId: string;
    /** Zero-based position in the host character list at read time (present on `list` results). */
    readonly index?: number;
    type?: 'character' | 'group' | (string & {});
    name?: string;
    image?: string;
    nickname?: string;
    desc?: string;
    personality?: string;
    scenario?: string;
    firstMessage?: string;
    alternateGreetings?: string[];
    notes?: string;
    creatorNotes?: string;
    systemPrompt?: string;
    postHistoryInstructions?: string;
    defaultVariables?: string;
    translatorNote?: string;
    tags?: string[];
    modules?: string[];
    characters?: string[];
    chatPage?: number;
    trashTime?: number;
}

/**
 * Character record returned by `risuai.characters`.
 * Never contains nested `chats` or `globalLore`. Unrequested or absent optional fields are omitted.
 */
export type PluginCharacterRecord = PluginCharacterKnownFields & {
    /** Host-specific extension fields preserved across partial updates. */
    [key: string]: unknown;
};

/** Input for creating a new character. */
export interface PluginCharacterCreateInput {
    name: string;
    type?: 'character' | 'group';
    desc?: string;
    personality?: string;
    scenario?: string;
    firstMessage?: string;
    alternateGreetings?: string[];
    notes?: string;
    image?: string;
    defaultVariables?: string;
    tags?: string[];
    modules?: string[];
    [key: string]: unknown;
}

/**
 * Partial update payload for `risuai.characters.update`.
 * - Known fields remain strictly typed; fields required by upstream storage types
 *   cannot be deleted with `null` (rejects with `INVALID_ARGUMENT`).
 * - `chaId`, `index`, `chats`, and `globalLore` are forbidden (`INVALID_ARGUMENT`).
 */
export type PluginCharacterUpdateInput = PluginRecordPatch<
    Omit<PluginCharacterKnownFields, 'chaId' | 'index'>,
    | 'name'
    | 'desc'
    | 'firstMessage'
    | 'notes'
    | 'chatPage'
    | 'creatorNotes'
    | 'systemPrompt'
    | 'postHistoryInstructions'
    | 'alternateGreetings'
    | 'tags'
    | 'personality'
    | 'scenario'
> & {
    chaId?: never;
    index?: never;
    chats?: never;
    globalLore?: never;
    [key: string]: unknown;
};

/** Options for `risuai.characters.list`. */
export interface PluginListCharactersOptions extends PluginListOptions<PluginCharacterField> {
    /** Include trashed/soft-deleted characters where the host supports a trash bin. Defaults to `false`. */
    includeTrashed?: boolean;
}

/** Sidebar folder entry in upstream's `characterOrder` shape. */
export interface PluginCharacterOrderFolder {
    id: string;
    name: string;
    color: string;
    data: string[];
    imgFile?: string;
    img?: string;
    [key: string]: unknown;
}

/** Entry in `characters.getOrder()` / `characters.setOrder()`, matching upstream `characterOrder`. */
export type PluginCharacterOrderEntry = string | PluginCharacterOrderFolder;

/** Known stored chat fields (`message`, `localLore`, and `scriptstate` are intentionally excluded). */
export type PluginChatField =
    | 'id'
    | 'name'
    | 'note'
    | 'bindedPersona'
    | 'lastDate'
    | 'folderId'
    | 'modules'
    | 'fmIndex'
    | 'bookmarks'
    | 'bookmarkNames'
    | 'suggestMessages'
    | 'sdData'
    | 'supaMemoryData'
    | 'hypaV2Data'
    | 'hypaV3Data'
    | 'lastMemory'
    | (string & {});

/** Strictly typed known fields of a chat record (no index signature). */
export interface PluginChatKnownFields {
    /** Durable unique chat identifier within its parent character. */
    readonly id: string;
    /** Zero-based position in the character's chat list at read time (present on `list` results). */
    readonly index?: number;
    name?: string;
    note?: string;
    bindedPersona?: string;
    lastDate?: number;
    folderId?: string;
    modules?: string[];
    fmIndex?: number;
    bookmarks?: string[];
    bookmarkNames?: Record<string, string>;
    suggestMessages?: string[];
    sdData?: string;
    supaMemoryData?: string;
    hypaV2Data?: unknown;
    hypaV3Data?: unknown;
    lastMemory?: string;
}

/**
 * Chat record returned by `risuai.chats`.
 * Never contains nested `message`, `localLore`, or `scriptstate`.
 * Cheap list fields across all hosts: `id`, `name`, `lastDate`, `folderId`, `modules`.
 */
export type PluginChatRecord = PluginChatKnownFields & {
    /** Host-specific extension fields preserved across partial updates. */
    [key: string]: unknown;
};

/** Input for creating a new chat inside a character. */
export interface PluginChatCreateInput {
    name?: string;
    note?: string;
    bindedPersona?: string;
    folderId?: string;
    modules?: string[];
    fmIndex?: number;
    [key: string]: unknown;
}

/**
 * Partial update payload for `risuai.chats.update`.
 * `id`, `index`, `message`, `localLore`, and `scriptstate` are forbidden (`INVALID_ARGUMENT`).
 */
export type PluginChatUpdateInput = PluginRecordPatch<
    Omit<PluginChatKnownFields, 'id' | 'index'>,
    'name' | 'note'
> & {
    id?: never;
    index?: never;
    message?: never;
    localLore?: never;
    scriptstate?: never;
    [key: string]: unknown;
};

/** Filter options for `risuai.chats.getVariables`. Variable names omit the internal `$` prefix. */
export interface PluginChatVariablesQueryOptions {
    /** Exact variable names (without leading `$`) to read. Missing names are omitted from the result. */
    names?: string[];
    /** Only include variables whose name (without leading `$`) starts with this prefix. */
    prefix?: string;
}

/**
 * Partial chat-variable patch for `risuai.chats.updateVariables`.
 * Keys omit the internal `$` prefix (e.g. `{ hp: '10', mood: null }`).
 * - `string | number | boolean`: sets `scriptstate['$' + name]`.
 * - `null`: removes `scriptstate['$' + name]` (idempotent if absent).
 * - `undefined`: ignored (no change).
 */
export type PluginChatVariablesUpdateInput = Record<string, PluginChatVariableValue | null | undefined>;

/** Known stored message fields. */
export type PluginMessageField =
    | 'messageId'
    | 'chatId'
    | 'role'
    | 'data'
    | 'time'
    | 'saying'
    | 'name'
    | 'disabled'
    | 'isComment'
    | 'generationInfo'
    | 'promptInfo'
    | (string & {});

/** Strictly typed known fields of a message record (no index signature). */
export interface PluginMessageKnownFields {
    /** Durable message identifier unique within the chat (always a non-empty string on hosts implementing `'chats'`). */
    readonly messageId: string;
    /** Upstream stored message ID field (preserved equal to `messageId`). */
    readonly chatId?: string;
    /** Zero-based position along the chat's active branch at read time. */
    readonly index: number;
    role?: PluginMessageRole;
    data?: string;
    time?: number;
    saying?: string;
    name?: string;
    disabled?: boolean | 'allBefore';
    isComment?: boolean;
    generationInfo?: Record<string, unknown>;
    promptInfo?: Record<string, unknown>;
}

/**
 * Message record returned by `risuai.messages`.
 * Includes explicit `messageId` (matching `PluginMessageTarget.messageId`) while
 * preserving upstream's stored `chatId` field for compatibility.
 */
export type PluginMessageRecord = PluginMessageKnownFields & {
    /** Host-specific extension fields (e.g. `swipes`, `swipeId`, `responseVariants`, custom plugin fields). */
    [key: string]: unknown;
};

/** Positional window options for `risuai.messages.list`. */
export interface PluginListMessagesPositionalOptions extends PluginListOptions<PluginMessageField> {
    anchorMessageId?: never;
    before?: never;
    after?: never;
}

/**
 * Anchored window options for `risuai.messages.list`.
 * - Mutually exclusive with `startIndex` and `pageSize` (`INVALID_ARGUMENT` if combined).
 * - `before` defaults to `0`, `after` defaults to `0`.
 * - The anchor message itself is ALWAYS included (returning up to `before + 1 + after` items in chronological order).
 * - If `before + 1 + after` exceeds the host window cap (at least 100), the host clamps
 *   `before`/`after` from the outer edges while keeping the anchor included (consistent with `pageSize` clamping).
 * - Rejects with `NOT_FOUND` if `anchorMessageId` does not exist on the active branch.
 */
export interface PluginListMessagesAnchoredOptions extends PluginReadFieldsOptions<PluginMessageField> {
    anchorMessageId: string;
    /** Non-negative safe integer count of messages before the anchor. Defaults to `0`. */
    before?: number;
    /** Non-negative safe integer count of messages after the anchor. Defaults to `0`. */
    after?: number;
    startIndex?: never;
    pageSize?: never;
}

/** Options for `risuai.messages.list` (positional mode or anchored mode). */
export type PluginListMessagesOptions =
    | PluginListMessagesPositionalOptions
    | PluginListMessagesAnchoredOptions;

/** Input for creating a new message in a chat. */
export interface PluginMessageCreateInput {
    role: PluginMessageRole;
    data: string;
    time?: number;
    saying?: string;
    name?: string;
    disabled?: boolean | 'allBefore';
    isComment?: boolean;
    [key: string]: unknown;
}

/** Insertion position for `risuai.messages.create`. Defaults to `{ at: 'end' }`. */
export type PluginMessageInsertPosition =
    | { at: 'end' }
    | { before: string }
    | { after: string };

/**
 * Partial update payload for `risuai.messages.update`.
 * - Known fields remain strictly typed (`role` MUST be `'user' | 'char' | undefined`; `data` MUST be `string | undefined`).
 * - `messageId`, `chatId`, and `index` are immutable (`INVALID_ARGUMENT`).
 * - Passing `null` on required fields (`role`, `data`) rejects with `INVALID_ARGUMENT`.
 */
export type PluginMessageUpdateInput = PluginRecordPatch<
    Omit<PluginMessageKnownFields, 'messageId' | 'chatId' | 'index'>,
    'role' | 'data'
> & {
    messageId?: never;
    chatId?: never;
    index?: never;
    [key: string]: unknown;
};

/** Options for single-call chat context reads (`risuai.chats.getContext`). */
export interface PluginChatContextOptions {
    /** Include selected character fields. */
    character?: PluginReadFieldsOptions<PluginCharacterField>;
    /** Include selected chat fields. */
    chat?: PluginReadFieldsOptions<PluginChatField>;
    /** Include a message window using the exact same options as `messages.list`. */
    messages?: PluginListMessagesOptions;
    /** Include chat variables using the exact same options as `chats.getVariables`. */
    variables?: PluginChatVariablesQueryOptions;
}

/** Result returned by `risuai.chats.getContext`. */
export interface PluginChatContextResult {
    characterId: string;
    chatId: string;
    character?: PluginCharacterRecord;
    chat?: PluginChatRecord;
    messages?: PluginListResult<PluginMessageRecord>;
    variables?: Record<string, PluginChatVariableValue>;
}

// ---------- `library` Records ----------

/** Known stored lorebook entry fields. */
export type PluginLorebookEntryField =
    | 'ref'
    | 'id'
    | 'key'
    | 'secondkey'
    | 'insertorder'
    | 'comment'
    | 'content'
    | 'mode'
    | 'alwaysActive'
    | 'selective'
    | 'useRegex'
    | 'activationPercent'
    | 'bookVersion'
    | 'folder'
    | 'extentions'
    | (string & {});

/** Strictly typed known fields of a lorebook entry (no index signature). */
export interface PluginLorebookEntryKnownFields {
    /** Always-present reference valid for `update`/`delete` during the current plugin run. Equals `id` when durable. */
    readonly ref: string;
    /** Durable stored entry ID, or `null` when the stored entry has no durable ID. */
    readonly id: string | null;
    /** Zero-based position within the owner's lorebook at read time. */
    readonly index: number;
    key?: string;
    secondkey?: string;
    insertorder?: number;
    comment?: string;
    content?: string;
    mode?: 'multiple' | 'constant' | 'normal' | 'child' | 'folder' | (string & {});
    alwaysActive?: boolean;
    selective?: boolean;
    useRegex?: boolean;
    activationPercent?: number;
    bookVersion?: number;
    folder?: string;
    extentions?: Record<string, unknown>;
}

/**
 * Lorebook entry record returned by `risuai.lorebookEntries`.
 * Separates session reference `ref` from durable stored `id`.
 */
export type PluginLorebookEntryRecord = PluginLorebookEntryKnownFields & {
    [key: string]: unknown;
};

/** Input for creating a lorebook entry. */
export interface PluginLorebookEntryCreateInput {
    key: string;
    content: string;
    secondkey?: string;
    insertorder?: number;
    comment?: string;
    mode?: 'multiple' | 'constant' | 'normal' | 'child' | 'folder';
    alwaysActive?: boolean;
    selective?: boolean;
    useRegex?: boolean;
    activationPercent?: number;
    folder?: string;
    extentions?: Record<string, unknown>;
    [key: string]: unknown;
}

/** Partial update payload for `risuai.lorebookEntries.update`. */
export type PluginLorebookEntryUpdateInput = PluginRecordPatch<
    Omit<PluginLorebookEntryKnownFields, 'ref' | 'id' | 'index'>,
    | 'key'
    | 'secondkey'
    | 'insertorder'
    | 'comment'
    | 'content'
    | 'mode'
    | 'alwaysActive'
    | 'selective'
> & {
    ref?: never;
    id?: never;
    index?: never;
    [key: string]: unknown;
};

/** Known stored module fields (nested `lorebook` is accessed via `risuai.lorebookEntries`). */
export type PluginModuleField =
    | 'id'
    | 'name'
    | 'description'
    | 'regex'
    | 'cjs'
    | 'trigger'
    | 'lowLevelAccess'
    | 'hideIcon'
    | 'backgroundEmbedding'
    | 'assets'
    | 'namespace'
    | 'customModuleToggle'
    | 'mcp'
    | (string & {});

/** Strictly typed known fields of a module record (no index signature). */
export interface PluginModuleKnownFields {
    readonly id: string;
    readonly index?: number;
    name?: string;
    description?: string;
    regex?: unknown[];
    cjs?: string;
    trigger?: unknown[];
    lowLevelAccess?: boolean;
    hideIcon?: boolean;
    backgroundEmbedding?: string;
    assets?: [string, string, string][];
    namespace?: string;
    customModuleToggle?: string;
    mcp?: Record<string, unknown>;
}

/** Module record returned by `risuai.modules`. Never contains nested `lorebook`. */
export type PluginModuleRecord = PluginModuleKnownFields & {
    [key: string]: unknown;
};

/** Input for creating a module. */
export interface PluginModuleCreateInput {
    name: string;
    description?: string;
    regex?: unknown[];
    cjs?: string;
    trigger?: unknown[];
    lowLevelAccess?: boolean;
    hideIcon?: boolean;
    backgroundEmbedding?: string;
    assets?: [string, string, string][];
    namespace?: string;
    customModuleToggle?: string;
    [key: string]: unknown;
}

/** Partial update payload for `risuai.modules.update`. */
export type PluginModuleUpdateInput = PluginRecordPatch<
    Omit<PluginModuleKnownFields, 'id' | 'index'>,
    'name' | 'description'
> & {
    id?: never;
    index?: never;
    lorebook?: never;
    [key: string]: unknown;
};

/** Known stored persona fields. */
export type PluginPersonaField =
    | 'ref'
    | 'id'
    | 'name'
    | 'personaPrompt'
    | 'icon'
    | 'largePortrait'
    | 'note'
    | 'characterId'
    | (string & {});

/** Strictly typed known fields of a persona record (no index signature). */
export interface PluginPersonaKnownFields {
    /** Always-present reference valid for `update`/`delete`/`setSelected` during the current plugin run. Equals `id` when durable. */
    readonly ref: string;
    /** Durable stored persona ID, or `null` when absent. */
    readonly id: string | null;
    /** Zero-based position at read time. */
    readonly index: number;
    name?: string;
    personaPrompt?: string;
    icon?: string;
    largePortrait?: boolean;
    note?: string;
    /** Populated on hosts that support character-scoped personas. */
    characterId?: string;
}

/** Persona record returned by `risuai.personas`. Separates session `ref` from durable stored `id`. */
export type PluginPersonaRecord = PluginPersonaKnownFields & {
    [key: string]: unknown;
};

/** Options for `risuai.personas.list`. */
export interface PluginListPersonasOptions extends PluginListOptions<PluginPersonaField> {
    /** Optional character filter on hosts that support character-scoped personas. */
    characterId?: string;
}

/** Input for creating a persona. */
export interface PluginPersonaCreateInput {
    name: string;
    personaPrompt: string;
    icon?: string;
    largePortrait?: boolean;
    note?: string;
    characterId?: string;
    [key: string]: unknown;
}

/** Partial update payload for `risuai.personas.update`. */
export type PluginPersonaUpdateInput = PluginRecordPatch<
    Omit<PluginPersonaKnownFields, 'ref' | 'id' | 'index'>,
    'name' | 'personaPrompt' | 'icon'
> & {
    ref?: never;
    id?: never;
    index?: never;
    [key: string]: unknown;
};

// ---------- `settings` Records ----------

/**
 * Closed allowlist of settings keys accessible via `risuai.settings`.
 * Requesting or updating any key outside this set rejects with `INVALID_ARGUMENT`.
 */
export interface PluginSettings {
    enabledModules: string[];
    moduleIntergration: string;
    temperature: number;
    askRemoval: boolean;
    maxContext: number;
    maxResponse: number;
    frequencyPenalty: number;
    PresensePenalty: number;
    theme: string;
    textTheme: string;
    lineHeight: number;
    seperateModelsForAxModels: boolean;
    seperateModels: {
        memory: string;
        emotion: string;
        translate: string;
        otherAx: string;
    };
    customCSS: string;
    guiHTML: string;
    colorSchemeName: string;
}

/** Closed union of allowed settings keys for `risuai.settings`. */
export type PluginSettingsKey = keyof PluginSettings;

/** Partial update payload for `risuai.settings.update` (all 16 allowlisted keys are required DB fields; `null` rejects with `INVALID_ARGUMENT`). */
export type PluginSettingsUpdateInput = Partial<PluginSettings>;

// ============================================================================
// 8. Selection, Lifecycle Events (`events`), and Change/Delete Events (`changes`)
// ============================================================================

/** Current UI selection expressed strictly by identifiers (never indexes). */
export interface PluginSelectionState {
    characterId: string | null;
    chatId: string | null;
    /** Durable ID of the effective persona (chat-bound if valid, else global), or `null` if none or no durable ID. */
    personaId: string | null;
    /** Session `ref` of the effective persona, or `null` when no persona is active. */
    personaRef: string | null;
}

/** Payload delivered to `risuai.selection.onChanged` (`events`). */
export interface PluginSelectionChangedEvent {
    current: PluginSelectionState;
    previous: PluginSelectionState;
}

/** Generation run trigger kind. */
export type PluginGenerationKind = 'send' | 'continue' | 'reroll';

/** Generation run initiator source. */
export type PluginGenerationSource = 'user' | 'plugin' | 'command';

/** Generation run terminal status. */
export type PluginGenerationStatus = 'completed' | 'failed' | 'aborted';

/** Payload delivered to `risuai.generation.onStarted` (`events`). */
export interface PluginGenerationStartedEvent {
    runId: string;
    characterId: string;
    chatId: string;
    kind: PluginGenerationKind;
    source: PluginGenerationSource;
    /** Calling plugin name when `source === 'plugin'`, otherwise `null`. */
    pluginName: string | null;
    /** User message ID sent by this run, or `null` for continue/reroll without new user input. */
    userMessageId: string | null;
    /** Main model identifier used for this run, or `null` if unavailable. */
    modelId: string | null;
}

/**
 * Payload delivered to `risuai.generation.onEnded` (`events`).
 *
 * When `status === 'completed'`, all messages listed in `messageIds` MUST be
 * immediately readable through `risuai.messages.list` / `risuai.messages.get`.
 */
export interface PluginGenerationEndedEvent {
    runId: string;
    characterId: string;
    chatId: string;
    kind: PluginGenerationKind;
    status: PluginGenerationStatus;
    /** Ordered IDs of messages written by this run (empty on abort/failure before commit). */
    messageIds: string[];
    /** Human-readable error message when `status === 'failed'`. */
    error?: string;
}

/** Payload delivered to `risuai.messages.onResponseReceived` (`events`). */
export interface PluginResponseReceivedEvent {
    runId: string | null;
    characterId: string;
    chatId: string;
    /** Committed response message ID, or `null` if removed by post-processing/triggers. */
    messageId: string | null;
    /** Zero-based index at delivery time, or `-1` if no longer present. */
    index: number;
}

/** Payload delivered to `risuai.chats.onDeleted` (`chats` + `changes`, permanent individual chat deletion only). */
export interface PluginChatDeletedEvent {
    characterId: string;
    chatId: string;
}

/**
 * Payload delivered to `risuai.characters.onDeleted` (`chats` + `changes`, permanent character deletion only).
 * Fires once for the character and does NOT fan out `chats.onDeleted` per child chat.
 */
export interface PluginCharacterDeletedEvent {
    characterId: string;
}

/**
 * Coalesced message removal event for `risuai.messages.onDeleted` (`chats` + `changes`).
 *
 * Hosts MUST emit this event when messages are removed from the stored chat via direct
 * or bulk deletion (falling back to `resync: true` when removals overflow or cannot be
 * enumerated). Unlike `chats.onDeleted` and `characters.onDeleted`, this event uses
 * relaxed semantics: hosts MAY also emit it on reroll or multi-message candidate switches
 * that remove previous IDs from storage, and reported `messageIds` MAY reappear later
 * (e.g. via undo or reroll navigation). Active-branch switches MUST NOT emit this event,
 * and parent chat/character deletions MUST NOT fan out `messages.onDeleted`. Never carries
 * message bodies.
 */
export interface PluginMessagesDeletedEvent {
    characterId: string;
    chatId: string;
    messageIds: string[];
    /** `true` when removals overflowed or could not be enumerated (e.g. full chat replacement). */
    resync: boolean;
}

/** Optional filter for `risuai.pluginStorage.onChanged` (`changes`). */
export interface PluginStorageChangeFilter {
    /** Exact keys to observe. */
    keys?: string[];
    /** Key prefix to observe. */
    prefix?: string;
}

/** Coalesced change event for `risuai.pluginStorage.onChanged` (`changes`). Never carries values. */
export interface PluginStorageChangedEvent {
    keys: string[];
    /** `true` when changes overflowed or could not be enumerated; callers should reload cached state. */
    resync: boolean;
}

/** Coalesced change event for single-ID collections (`characters`: `chats` + `changes`; `modules`, `personas`: `library` + `changes`). */
export interface PluginCollectionChangedEvent {
    ids: string[];
    resync: boolean;
}

/** Coalesced change event for `risuai.chats.onChanged` (includes message mutations within those chats) (`chats` + `changes`). */
export interface PluginChatCollectionChangedEvent {
    targets: PluginChatTarget[];
    resync: boolean;
}

/** Coalesced change event for `risuai.settings.onChanged` (`settings` + `changes`). */
export interface PluginSettingsChangedEvent {
    keys: PluginSettingsKey[];
    resync: boolean;
}

// ============================================================================
// 9. Optional Extensions: `inlayMedia`, `messageSlots` (`chatView`), `mcp`
// ============================================================================

/** Input data accepted by `risuai.createInlay` (matches upstream V3 `Uint8Array | string`). */
export type PluginInlayInputData = Uint8Array | string;

/** Options for `risuai.createInlay`. */
export interface PluginCreateInlayOptions {
    /** Filename with extension used to infer asset type (e.g. `'clip.mp3'`, `'pic.png'`). */
    name?: string;
}

/** Placement of a host-owned message slot (`messageSlots`). */
export type PluginMessageSlotPosition = 'aboveBody' | 'belowBody';

/** Registration options for `risuai.chatView.registerMessageSlot` (`messageSlots`). */
export interface PluginMessageSlotOptions {
    position: PluginMessageSlotPosition;
    /** CSS stylesheet scoped exclusively to this plugin's shadow-root slots. Host `--risu-theme-*` CSS variables inherit. */
    style?: string;
}

/** In-place update options passed to `slotHandle.update(options)`. */
export interface PluginMessageSlotUpdateOptions {
    style?: string;
}

/** Message row render lifecycle payload (`chatView.onMessageRendered` / `onMessageRemoved`) (`messageSlots`). */
export interface PluginChatViewMessageEvent {
    characterId: string;
    chatId: string;
    messageId: string;
    index: number;
    role: PluginMessageRole;
}

/** Click event dispatched when a user clicks an element with `data-action` inside a message slot (`messageSlots`). */
export interface PluginMessageSlotActionEvent {
    slotId: string;
    characterId: string;
    chatId: string;
    messageId: string;
    /** Value of the clicked element's `data-action` attribute. */
    action: string;
    /** All `data-*` attributes present on the clicked element. */
    dataset: Record<string, string>;
}

/** Target identifying an enabled MCP module for a specific chat (`mcp`). */
export interface PluginMCPModuleTarget extends PluginChatTarget {
    /** Module ID returned by `risuai.mcp.list(target)`. */
    mcpId: string;
}

/** Summary of an enabled MCP module returned by `risuai.mcp.list` (never exposes MCP URL/address). */
export interface PluginMCPModuleSummary {
    id: string;
    name: string;
    version: string;
    description: string;
}

/** Options for `risuai.mcp.callTool`. */
export interface PluginMCPCallToolOptions {
    signal?: AbortSignal;
}

// ============================================================================
// 10. Extension Namespace Interfaces
// ============================================================================

/** `risuai.selection` namespace (`chats`, `events`). */
export interface PluginSelectionNamespace {
    /** Returns the currently selected character, chat, and effective persona by ID/ref. Requires `db`. (`chats`) */
    get(): Promise<PluginSelectionState>;
    /** Fires when selected IDs/refs change; never fires on index shifts alone. Requires `db`. (`events`) */
    onChanged(listener: (event: PluginSelectionChangedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.characters` namespace (`chats`, `changes`). */
export interface PluginCharactersNamespace {
    /** Reads one character by ID (without nested `chats` or `globalLore`), or `null` if absent. Requires `db`. (`chats`) */
    get(target: PluginCharacterTarget, options?: PluginReadFieldsOptions<PluginCharacterField>): Promise<PluginCharacterRecord | null>;
    /** Pages characters in host order. Default `fields`: `['chaId', 'name', 'type', 'image']`. Requires `db`. (`chats`) */
    list(options?: PluginListCharactersOptions): Promise<PluginListResult<PluginCharacterRecord>>;
    /** Creates a new character and returns it with its host-assigned `chaId`. Requires `dbWrite`. (`chats`) */
    create(data: PluginCharacterCreateInput): Promise<PluginCharacterRecord>;
    /** Partially updates named character fields (`null` removes optional keys; `undefined` ignored). Requires `dbWrite`. (`chats`) */
    update(target: PluginCharacterTarget, changes: PluginCharacterUpdateInput): Promise<void>;
    /** Deletes a character by ID. Rejects `NOT_FOUND` if absent. Requires `dbWrite`. (`chats`) */
    delete(target: PluginCharacterTarget): Promise<void>;
    /** Reads sidebar character and folder order in upstream `characterOrder` shape. Requires `db`. (`chats`) */
    getOrder(): Promise<PluginCharacterOrderEntry[]>;
    /** Replaces sidebar character and folder order. Requires `dbWrite`. (`chats`) */
    setOrder(order: PluginCharacterOrderEntry[]): Promise<void>;
    /** Fires coalesced character ID changes (never values). Requires `db`. (`chats` + `changes`) */
    onChanged(listener: (event: PluginCollectionChangedEvent) => void | Promise<void>): Promise<PluginHandle>;
    /** Fires once on permanent deletion of a character (not trash, and does not fan out `chats.onDeleted`). Requires `db`. (`chats` + `changes`) */
    onDeleted(listener: (event: PluginCharacterDeletedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.chats` namespace (`chats`, `changes`). */
export interface PluginChatsNamespace {
    /** Reads one chat record (without `message`, `localLore`, or `scriptstate`), or `null` if absent. Requires `db`. (`chats`) */
    get(target: PluginChatTarget, options?: PluginReadFieldsOptions<PluginChatField>): Promise<PluginChatRecord | null>;
    /** Pages chats of a character. Default `fields`: `['id', 'name', 'lastDate', 'folderId', 'modules']`. Rejects `NOT_FOUND` if character does not exist. Requires `db`. (`chats`) */
    list(target: PluginCharacterTarget, options?: PluginListOptions<PluginChatField>): Promise<PluginListResult<PluginChatRecord>>;
    /** Creates a new chat inside a character and returns it with its host-assigned `id`. Requires `dbWrite`. (`chats`) */
    create(target: PluginCharacterTarget, data?: PluginChatCreateInput): Promise<PluginChatRecord>;
    /** Partially updates named chat metadata fields (`message`, `localLore`, `scriptstate` forbidden). Requires `dbWrite`. (`chats`) */
    update(target: PluginChatTarget, changes: PluginChatUpdateInput): Promise<void>;
    /** Deletes a chat by ID. Rejects `NOT_FOUND` if absent. Requires `dbWrite`. (`chats`) */
    delete(target: PluginChatTarget): Promise<void>;
    /**
     * Reads selected character fields, chat fields, a message window, and/or chat variables
     * in a single RPC with coherent target resolution. Pass `null` as `target` to resolve the
     * currently selected chat. Returns `null` when the target (or active selection) does not exist.
     * Requires `db`. (`chats`)
     */
    getContext(target: PluginChatTarget | null, options?: PluginChatContextOptions): Promise<PluginChatContextResult | null>;
    /**
     * Reads chat variables (`scriptstate`) by exact names and/or prefix, omitting the internal `$` prefix.
     * Absent names are omitted from the returned object. Rejects `NOT_FOUND` if target chat does not exist.
     * Requires `db`. (`chats`)
     */
    getVariables(target: PluginChatTarget, options?: PluginChatVariablesQueryOptions): Promise<Record<string, PluginChatVariableValue>>;
    /**
     * Partially updates chat variables (`scriptstate`) by name without the internal `$` prefix.
     * `null` removes a key; `undefined` is ignored. Rejects `NOT_FOUND` if target chat does not exist.
     * Requires `dbWrite`. (`chats`)
     */
    updateVariables(target: PluginChatTarget, changes: PluginChatVariablesUpdateInput): Promise<void>;
    /** Fires coalesced chat target changes (including message changes; never values). Requires `db`. (`chats` + `changes`) */
    onChanged(listener: (event: PluginChatCollectionChangedEvent) => void | Promise<void>): Promise<PluginHandle>;
    /** Fires on permanent deletion of an individual chat (not fanned out on character delete). Requires `db`. (`chats` + `changes`) */
    onDeleted(listener: (event: PluginChatDeletedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.messages` namespace (`chats`, `events`, `changes`). */
export interface PluginMessagesNamespace {
    /** Reads one message by ID, or `null` if absent on the active branch. Requires `db`. (`chats`) */
    get(target: PluginMessageTarget, options?: PluginReadFieldsOptions<PluginMessageField>): Promise<PluginMessageRecord | null>;
    /** Pages messages along the chat's active branch (positional or anchored mode). Rejects `NOT_FOUND` if target chat or anchor does not exist. Requires `db`. (`chats`) */
    list(target: PluginChatTarget, options?: PluginListMessagesOptions): Promise<PluginListResult<PluginMessageRecord>>;
    /** Inserts a message at the end or before/after a target message ID. Requires `dbWrite`. (`chats`) */
    create(target: PluginChatTarget, data: PluginMessageCreateInput, position?: PluginMessageInsertPosition): Promise<PluginMessageRecord>;
    /** Partially updates named fields of a message, preserving omitted and host-extension fields. Requires `dbWrite`. (`chats`) */
    update(target: PluginMessageTarget, changes: PluginMessageUpdateInput): Promise<void>;
    /** Deletes a message by ID. May reject `BRANCH_CONFLICT` on branching hosts. Requires `dbWrite`. (`chats`) */
    delete(target: PluginMessageTarget): Promise<void>;
    /** Fires after a model response is committed to a chat, carrying IDs only. Requires `db`. (`events`) */
    onResponseReceived(listener: (event: PluginResponseReceivedEvent) => void | Promise<void>): Promise<PluginHandle>;
    /**
     * MUST fire when messages are removed from the stored chat by direct or bulk deletion;
     * MAY also fire on reroll or candidate switches that remove stored IDs (relaxed semantics:
     * reported IDs MAY reappear via undo/reroll). Active-branch switches and parent
     * chat/character deletions MUST NOT emit this event. Never carries message bodies.
     * Requires `db`. (`chats` + `changes`)
     */
    onDeleted(listener: (event: PluginMessagesDeletedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.generation` namespace (`events`). */
export interface PluginGenerationNamespace {
    /** Fires when a generation run starts (send, continue, or reroll). Requires `db`. (`events`) */
    onStarted(listener: (event: PluginGenerationStartedEvent) => void | Promise<void>): Promise<PluginHandle>;
    /** Fires once when a generation run finishes (`completed`, `failed`, or `aborted`). Requires `db`. (`events`) */
    onEnded(listener: (event: PluginGenerationEndedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.lorebookEntries` namespace (`library`). */
export interface PluginLorebookEntriesNamespace {
    /** Reads one lorebook entry by owner and `ref`, or `null` if absent. Requires `db`. (`library`) */
    get(target: PluginLorebookEntryTarget, options?: PluginReadFieldsOptions<PluginLorebookEntryField>): Promise<PluginLorebookEntryRecord | null>;
    /** Pages lorebook entries belonging to an owner. Rejects `NOT_FOUND` if owner does not exist. Requires `db`. (`library`) */
    list(owner: PluginLorebookOwner, options?: PluginListOptions<PluginLorebookEntryField>): Promise<PluginListResult<PluginLorebookEntryRecord>>;
    /** Creates a new lorebook entry for an owner and returns its record (with `ref` and optional durable `id`). Requires `dbWrite`. (`library`) */
    create(owner: PluginLorebookOwner, data: PluginLorebookEntryCreateInput): Promise<PluginLorebookEntryRecord>;
    /** Partially updates a lorebook entry by `ref`. Rejects `NOT_FOUND` or `CONFLICT` if `ref` is stale/ambiguous. Requires `dbWrite`. (`library`) */
    update(target: PluginLorebookEntryTarget, changes: PluginLorebookEntryUpdateInput): Promise<void>;
    /** Deletes a lorebook entry by `ref`. Rejects `NOT_FOUND` or `CONFLICT` if `ref` is stale/ambiguous. Requires `dbWrite`. (`library`) */
    delete(target: PluginLorebookEntryTarget): Promise<void>;
}

/** `risuai.modules` namespace (`library`, `changes`). */
export interface PluginModulesNamespace {
    /** Reads one module (without nested `lorebook`), or `null` if absent. Requires `db`. (`library`) */
    get(target: PluginModuleTarget, options?: PluginReadFieldsOptions<PluginModuleField>): Promise<PluginModuleRecord | null>;
    /** Pages installed modules. Requires `db`. (`library`) */
    list(options?: PluginListOptions<PluginModuleField>): Promise<PluginListResult<PluginModuleRecord>>;
    /** Creates a module and returns it with its host-assigned `id`. Requires `dbWrite`. (`library`) */
    create(data: PluginModuleCreateInput): Promise<PluginModuleRecord>;
    /** Partially updates named module fields. Requires `dbWrite`. (`library`) */
    update(target: PluginModuleTarget, changes: PluginModuleUpdateInput): Promise<void>;
    /** Permanently deletes a module by ID. Requires `dbWrite`. (`library`) */
    delete(target: PluginModuleTarget): Promise<void>;
    /** Fires coalesced module ID changes (never values). Requires `db`. (`library` + `changes`) */
    onChanged(listener: (event: PluginCollectionChangedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.personas` namespace (`library`, `changes`). */
export interface PluginPersonasNamespace {
    /** Reads one persona by `ref`, or `null` if absent. Requires `db`. (`library`) */
    get(target: PluginPersonaTarget, options?: PluginReadFieldsOptions<PluginPersonaField>): Promise<PluginPersonaRecord | null>;
    /** Pages user personas. Requires `db`. (`library`) */
    list(options?: PluginListPersonasOptions): Promise<PluginListResult<PluginPersonaRecord>>;
    /** Creates a persona and returns its record (with `ref` and optional durable `id`). Requires `dbWrite`. (`library`) */
    create(data: PluginPersonaCreateInput): Promise<PluginPersonaRecord>;
    /** Partially updates a persona by `ref`. Rejects `NOT_FOUND` or `CONFLICT` if `ref` is stale. Requires `dbWrite`. (`library`) */
    update(target: PluginPersonaTarget, changes: PluginPersonaUpdateInput): Promise<void>;
    /** Deletes a persona by `ref`. Rejects `NOT_FOUND` or `CONFLICT` if `ref` is stale. Requires `dbWrite`. (`library`) */
    delete(target: PluginPersonaTarget): Promise<void>;
    /** Reads the globally selected persona, or `null` if none. Requires `db`. (`library`) */
    getSelected(): Promise<PluginPersonaRecord | null>;
    /** Sets the globally selected persona by `ref`. Requires `dbWrite`. (`library`) */
    setSelected(target: PluginPersonaTarget): Promise<void>;
    /** Fires coalesced persona ID/ref changes (never values). Requires `db`. (`library` + `changes`) */
    onChanged(listener: (event: PluginCollectionChangedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.settings` namespace (`settings`, `changes`). */
export interface PluginSettingsNamespace {
    /** Reads only the requested allowlisted settings keys. Keys outside `PluginSettingsKey` reject with `INVALID_ARGUMENT`. Requires `db`. (`settings`) */
    get<K extends PluginSettingsKey>(keys: K[]): Promise<Partial<Pick<PluginSettings, K>>>;
    /** Partially updates allowlisted settings keys (`undefined` ignored; unknown keys or `null` on required keys reject with `INVALID_ARGUMENT`). Requires `dbWrite`. (`settings`) */
    update(changes: PluginSettingsUpdateInput): Promise<void>;
    /** Fires coalesced settings key changes (never values). Requires `db`. (`settings` + `changes`) */
    onChanged(listener: (event: PluginSettingsChangedEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.chatView` namespace (`messageSlots`). */
export interface PluginChatViewNamespace {
    /** Fires when a finalized message row is mounted or re-rendered in the active chat view. Requires `db`. (`messageSlots`) */
    onMessageRendered(listener: (event: PluginChatViewMessageEvent) => void | Promise<void>): Promise<PluginHandle>;
    /** Fires when a message row leaves the active chat view. Requires `db`. (`messageSlots`) */
    onMessageRemoved(listener: (event: PluginChatViewMessageEvent) => void | Promise<void>): Promise<PluginHandle>;
    /**
     * Reserves a host-owned shadow-root slot above or below each message body.
     * `handle.update({ style })` updates the shared stylesheet for this slot. No permission prompt. (`messageSlots`)
     */
    registerMessageSlot(options: PluginMessageSlotOptions): Promise<PluginUpdatableHandle<[options: PluginMessageSlotUpdateOptions]>>;
    /** Sets sanitized HTML for one message inside an owned slot (kept in memory for the active chat). (`messageSlots`) */
    setSlotContent(slotId: string, target: PluginMessageTarget, html: string): Promise<void>;
    /** Clears HTML for one message inside an owned slot. (`messageSlots`) */
    clearSlotContent(slotId: string, target: PluginMessageTarget): Promise<void>;
    /** Returns the mounted slot container as a `SafeElement`, or `null` when the row is not currently mounted. (`messageSlots`) */
    getSlotElement(slotId: string, target: PluginMessageTarget): Promise<PluginSafeElementRef | null>;
    /** Subscribes to clicks on elements carrying a `data-action` attribute inside an owned slot. (`messageSlots`) */
    onSlotAction(slotId: string, listener: (event: PluginMessageSlotActionEvent) => void | Promise<void>): Promise<PluginHandle>;
}

/** `risuai.mcp` namespace (`mcp`). */
export interface PluginMCPNamespace {
    /** Lists MCP modules enabled for the target chat (without exposing MCP URLs). Requires `mcpAccess`. (`mcp`) */
    list(target: PluginChatTarget): Promise<PluginListResult<PluginMCPModuleSummary>>;
    /** Lists tools exposed by one enabled MCP module for the target chat. Requires `mcpAccess`. (`mcp`) */
    listTools(target: PluginMCPModuleTarget): Promise<PluginListResult<PluginMCPToolDef>>;
    /** Invokes one tool on an enabled MCP module for the target chat. Requires `mcpAccess`. (`mcp`) */
    callTool(
        target: PluginMCPModuleTarget,
        toolName: string,
        args: Record<string, unknown>,
        options?: PluginMCPCallToolOptions,
    ): Promise<PluginMCPToolCallContent[]>;
}

// ============================================================================
// 11. Storage & Complete Extension API Surface
// ============================================================================

/** Extended `risuai.pluginStorage` surface (`storage.pluginStorage.bytes`, `storageUpdate`, `changes`). */
export interface PluginStorageExtensions {
    /**
     * Stores a JSON-serializable value, or a top-level `Uint8Array` / `ArrayBuffer`
     * when `getRuntimeInfo().storage.pluginStorage.bytes === true`.
     */
    setItem(key: string, value: unknown): Promise<void>;
    /** Reads a stored value (or `null` if absent). */
    getItem(key: string): Promise<unknown>;
    removeItem(key: string): Promise<void>;
    clear(): Promise<void>;
    key(index: number): Promise<string | null>;
    keys(): Promise<string[]>;
    length(): Promise<number>;
    /**
     * Conflict-detecting read-modify-write on one key (`storageUpdate`).
     * The host may re-run `updater` with the latest value upon concurrent modification,
     * rejecting with `CONFLICT` if contention persists. Returning `null` deletes the key.
     */
    update<T = unknown>(
        key: string,
        updater: (current: T | null) => T | null | Promise<T | null>,
    ): Promise<T | null>;
    /** Observes coalesced key changes on this plugin's keys (never values) (`changes`). */
    onChanged(
        filter: PluginStorageChangeFilter | undefined,
        listener: (event: PluginStorageChangedEvent) => void | Promise<void>,
    ): Promise<PluginHandle>;
}

/**
 * Local plugin storage returned by `risuai.getLocalPluginStorage()`.
 * Check `getRuntimeInfo().storage.localPluginStorage` for `scope` (`'isolated' | 'shared'`),
 * `location` (`'device' | 'server'`), and `structured` (binary & structured-clone support).
 */
export interface PluginLocalStorage {
    getItem<T = unknown>(key: string): Promise<T | null>;
    setItem<T = unknown>(key: string, value: T): Promise<void>;
    removeItem(key: string): Promise<void>;
    keys(): Promise<string[]>;
    clear(): Promise<void>;
}

/**
 * Complete normative surface of RisuAI Plugin API v3 Extensions.
 *
 * Host implementers expose the applicable namespaces and extended V3 methods on
 * the global `risuai` object, gated by `getRuntimeInfo().features`.
 */
export interface RisuaiPluginExtensionsAPI {
    // ---------- Runtime Discovery & Storage ----------
    getRuntimeInfo(): Promise<PluginRuntimeInfo>;
    requestPluginPermission(permission: PluginPermissionName): Promise<boolean>;
    readonly pluginStorage: PluginStorageExtensions;
    getLocalPluginStorage(): Promise<PluginLocalStorage>;

    // ---------- New Top-Level Extension Namespaces ----------
    readonly selection: PluginSelectionNamespace;
    readonly characters: PluginCharactersNamespace;
    readonly chats: PluginChatsNamespace;
    readonly messages: PluginMessagesNamespace;
    readonly generation: PluginGenerationNamespace;
    readonly lorebookEntries: PluginLorebookEntriesNamespace;
    readonly modules: PluginModulesNamespace;
    readonly personas: PluginPersonasNamespace;
    readonly settings: PluginSettingsNamespace;
    readonly chatView: PluginChatViewNamespace;
    readonly mcp: PluginMCPNamespace;

    // ---------- Extended Existing V3 Methods ----------

    /** Extended `runLLMModel` (`modelOptions`). */
    runLLMModel(options: PluginRunLLMModelOptions): Promise<unknown>;

    /** Extended `nativeFetch` (`fetchOptions` and optional `fetchRedirect`). */
    nativeFetch(url: string, options?: PluginNativeFetchOptions): Promise<Response>;

    /** `addRisuReplacer` with handle return (`handles`) and opt-in `requestContext`. Resolves `null` on declined permission. */
    addRisuReplacer(
        type: 'beforeRequest',
        func: (
            messages: PluginOpenAIChatMessage[],
            type: string,
            context?: PluginRequestContext,
        ) => PluginOpenAIChatMessage[] | Promise<PluginOpenAIChatMessage[]>,
        options?: PluginRequestContextOptions,
    ): Promise<PluginHandle | null>;
    addRisuReplacer(
        type: 'afterRequest',
        func: (
            content: string,
            type: string,
            context?: PluginRequestContext,
        ) => string | Promise<string>,
        options?: PluginRequestContextOptions,
    ): Promise<PluginHandle | null>;

    /** `registerBodyIntercepter` with handle return (`handles`) and opt-in `requestContext`. Resolves `null` on declined permission. */
    registerBodyIntercepter(
        callback: (body: any, type: string, context?: PluginRequestContext) => any,
        options?: PluginRequestContextOptions,
    ): Promise<PluginHandle | null>;

    /**
     * `addProvider` with handle return (`handles`) and opt-in `requestContext`.
     * Matches V3 permission timing: `provider` permission is checked when the provider is invoked,
     * not at registration, so registration always resolves to a `PluginHandle`.
     */
    addProvider(
        name: string,
        func: (
            args: Record<string, any>,
            abortSignal?: AbortSignal,
            context?: PluginRequestContext,
        ) => Promise<{ success: boolean; content: string | ReadableStream<string> }>,
        options?: PluginProviderOptions,
    ): Promise<PluginHandle>;

    /** `addRisuChatListener` returning a handle (`handles`). Resolves `null` on declined permission. */
    addRisuChatListener(
        mode: 'output',
        func: (arg: {
            char: any;
            chat: any;
            characterIndex: number;
            chatIndex: number;
            messageIndex: number;
        }) => void | Promise<void>,
    ): Promise<PluginHandle | null>;

    /** `registerSetting` returning an updatable handle (`handles`). */
    registerSetting(
        name: string,
        callback: () => void | Promise<void>,
        icon?: string,
        iconType?: PluginIconType,
        id?: string,
    ): Promise<
        PluginUpdatableHandle<
            [name: string, callback: () => void | Promise<void>, icon?: string, iconType?: PluginIconType]
        >
    >;

    /** `registerButton` with optional `location: 'message'` (`messageButtons`) and updatable handle (`handles`). */
    registerButton(
        arg: {
            name: string;
            icon: string;
            iconType: PluginIconType;
            location?: 'action' | 'chat' | 'hamburger' | 'message';
            id?: string;
            roles?: PluginMessageRole[];
        },
        callback: (target?: PluginMessageButtonTarget) => void | Promise<void>,
    ): Promise<
        PluginUpdatableHandle<
            [
                arg: {
                    name: string;
                    icon: string;
                    iconType: PluginIconType;
                    roles?: PluginMessageRole[];
                },
                callback: (target?: PluginMessageButtonTarget) => void | Promise<void>,
            ]
        >
    >;

    /** Formally declared V3 chat panel API returning an updatable handle (`handles`). */
    setChatPanel(
        content: string | null,
        options?: {
            id?: string;
            className?: string;
        },
    ): Promise<PluginUpdatableHandle<[content: string | null, options?: { className?: string }]>>;

    /** `registerMCP` returning a handle (`handles`). */
    registerMCP(
        arg: {
            identifier: string;
            name: string;
            version: string;
            description: string;
        },
        getToolList: () => Promise<PluginMCPToolDef[]>,
        callTool: (toolName: string, content: any) => Promise<PluginMCPToolCallContent[]>,
    ): Promise<PluginHandle>;

    /** `addTTSPreprocessor` returning a handle (`handles`). */
    addTTSPreprocessor(
        func: (ctx: { text: string; ttsMode: string; characterId: string }) =>
            | Promise<{ text?: string; skip?: boolean } | void>
            | { text?: string; skip?: boolean }
            | void,
    ): Promise<PluginHandle>;

    /** `addTTSPostprocessor` returning a handle (`handles`). */
    addTTSPostprocessor(
        func: (ctx: { audio: ArrayBuffer; mimeType: string; ttsMode: string; characterId: string }) =>
            | Promise<{ audio?: ArrayBuffer; mimeType?: string; skip?: boolean } | void>
            | { audio?: ArrayBuffer; mimeType?: string; skip?: boolean }
            | void,
    ): Promise<PluginHandle>;

    /** `addRisuScriptHandler` returning a handle (`handles`). */
    addRisuScriptHandler(
        mode: PluginScriptMode,
        func: (content: string) => string | null | undefined | Promise<string | null | undefined>,
    ): Promise<PluginHandle>;

    /** `addPluginChannelListener` returning a handle (`handles`). */
    addPluginChannelListener(
        channelName: string,
        callback: (message: any, meta: { sender: string; channel: string }) => void | Promise<void>,
    ): Promise<PluginHandle>;

    /** `onUnload` returning a handle (`handles`). */
    onUnload(func: () => void | Promise<void>): Promise<PluginHandle>;

    // ---------- Formally Declared V3 Dialog Helpers ----------
    alert(msg: string): Promise<void>;
    alertConfirm(msg: string): Promise<boolean>;
    alertError(msg: string): Promise<void>;

    // ---------- Inlay Write & Delete (`createInlay` / `inlayMedia`) ----------
    /**
     * Preserves upstream V3 `createInlay` semantics: returns the host-assigned inlay ID
     * string on success, or `null` if `inlay` permission is declined.
     * Supports images under `createInlay`, and additionally audio/video under `inlayMedia`.
     */
    createInlay(
        data: PluginInlayInputData,
        options?: PluginCreateInlayOptions,
    ): Promise<string | null>;

    /** Deletes an inlay asset by ID (`inlayMedia`). Rejects `NOT_FOUND` if absent or `PERMISSION_DENIED` if declined. Requires `inlay` permission. */
    deleteInlay(id: string): Promise<void>;
}
