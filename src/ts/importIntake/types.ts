import type { AndroidSpoolReady } from '../storage/androidSafBridge'

/** Where one file of a delivery comes from. */
export type ImportIntakeSource =
    /** A file dropped on the window. `inboxPath` is its copy in content-import-inbox, once made. */
    | { type: 'file'; file: File; inboxPath?: string }
    /** A desktop or iOS opened file, or an Android opened file outside the spool allowlist. */
    | { type: 'path'; path: string }
    | { type: 'androidSpool'; spool: AndroidSpoolReady }
    /** A file that could not be prepared for import; it is always refused. */
    | { type: 'failed'; name: string; code: string }

export type ImportIntakeOrigin = 'drop' | 'opened'

/** Files that arrived together: one drop, or one file association or share. */
export interface ImportIntakeDelivery {
    origin: ImportIntakeOrigin
    sources: ImportIntakeSource[]
}

export type ImportIntakeKind =
    | 'character' | 'module' | 'lorebook' | 'preset' | 'backup' | 'plugin' | 'unsupported'

/** Wire values of the native classifier. */
export type ImportIntakeFormat =
    | 'png' | 'charx' | 'jpeg-charx' | 'json'
    | 'risum' | 'risup' | 'risupreset'
    | 'portable' | 'block-risu-save' | 'local-backup'
    | 'js' | 'ts'

export type ImportIntakeRefusal =
    | { code: 'unsupported-format' }
    | { code: 'plain-jpeg' }
    | { code: 'png-without-card' }
    | { code: 'backup-with-other-files' }
    | { code: 'plugin-api-version'; version: string }
    /** Reading or classifying failed; `error` goes through the import failure reason mapping. */
    | { code: 'read-failed'; error: unknown }

export interface ImportIntakePluginHeader {
    name: string
    displayName?: string
    version?: string
    apiVersion: string
    /** Host of `//@update-url`, never the full URL. */
    updateHost?: string
}

export interface ImportIntakeItem {
    /** Unique within a delivery. */
    id: string
    name: string
    bytes?: number
    source: ImportIntakeSource
    kind: ImportIntakeKind
    format?: ImportIntakeFormat
    /** Present exactly when the file cannot be imported in this delivery. */
    refusal?: ImportIntakeRefusal
    /** CharX only: the name or, for drops, the open screen points at a module. */
    moduleHint?: boolean
    /** Plugin only. */
    plugin?: { header: ImportIntakePluginHeader; code: string; isTypescript: boolean }
}

export type ImportIntakeLorebookTarget =
    | { type: 'character'; chaId: string; name: string }
    | { type: 'new-module' }

export interface ImportSheetRowDecision {
    id: string
    /** CharX rows. */
    charxAs?: 'character' | 'module'
    /** Lorebook rows. */
    lorebookTarget?: ImportIntakeLorebookTarget
}

export interface ImportSheetRequest {
    items: ImportIntakeItem[]
    /** The selected character, when one is selected; offered for lorebook rows. */
    lorebookCharacter?: { chaId: string; name: string }
    /** Default per CharX and lorebook row id. */
    defaults: Record<string, Omit<ImportSheetRowDecision, 'id'>>
    /** Opens the plugin dialog for a plugin row; resolves true once installed. */
    installPlugin(item: ImportIntakeItem): Promise<boolean>
}

/** The checked importable rows, in delivery order; null when the user cancelled or closed. */
export type ImportSheetDecision = ImportSheetRowDecision[] | null
