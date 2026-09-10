/// <reference types="svelte" />
/// <reference types="vite/client" />

interface ImportMetaEnv {
    readonly VITE_RUNTIME_PERFORMANCE_PROFILE?: 'normal' | 'low-spec'
}

interface ImportMeta {
    readonly env: ImportMetaEnv
}

declare var Buffer: BufferConstructor
declare var safeStructuredClone: <T>(data: T) => T
declare var userScriptFetch: (url: string, arg: RequestInit) => Promise<Response>
