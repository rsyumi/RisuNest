import { untrack } from 'svelte'
import { fromStore } from 'svelte/store'
import { DBState } from '../stores.svelte'
import type { Database } from './database.svelte'
import { safeStructuredClone } from '../safeStructuredClone'
import { markPresetEditorSurface, settingsWindowStore } from '../gui/presetEditorSurface'
import {
    createPresetMirrorView,
    deriveEffectivePresetMirrors,
    derivePresetMirrorView,
    flushEffectivePresetEdits,
    flushPresetMirrorView,
    peekEffectivePresetId,
    presetMirrorMap,
    protectedPresetGroups,
    type ApplyPreset,
    type PresetMirrorView,
    type ProtectedPresetField,
} from './effectiveIdentityState'

type Fields = Record<string, unknown>
interface EditorView extends PresetMirrorView {
    db: Database
    dispose(): void
}

// The identity state lives in a WeakMap, so its changes reach components through these counters.
const signal = $state({ view: 0, applied: 0, surfaces: 0 })
let view: EditorView | null = null
let applied: { db: Database; id: string | undefined } | null = null
let settingsWindow: { readonly current: boolean } | undefined
let watchingSettingsWindow = false
let applyEditor: ApplyPreset | undefined
const editors = new WeakMap<Database, Database>()

const clone = <T>(value: T): T => value === undefined ? value : safeStructuredClone(value)

/** Whether preset settings show: the settings window, or a surface such as quick settings that holds the editor. */
export function isPresetEditorOpen(): boolean {
    settingsWindow ??= fromStore(settingsWindowStore())
    return settingsWindow.current || signal.surfaces > 0
}

function track(value: unknown): void {
    if (!value || typeof value !== 'object') return
    for (const key in value as Fields) track((value as Fields)[key])
}

function closeView(): void {
    if (!view) return
    const current = view
    if (current.db === DBState.db) untrack(() => flushPresetMirrorView(current.db, current))
    view.dispose()
    view = null
    signal.view++
}

function openView(db: Database, presetId: string, apply: ApplyPreset): void {
    const created = createPresetMirrorView(db, presetId, apply)
    if (!created) return
    const values = $state(created.values)
    const next: EditorView = { ...created, values, db, copy: (value) => $state.snapshot(value), dispose: () => {} }
    // Edits reach the record as they happen, which the save observer then picks up.
    next.dispose = $effect.root(() => {
        for (const key of Object.keys(presetMirrorMap)) {
            $effect(() => {
                track(values[key])
                untrack(() => {
                    if (view === next && next.db === DBState.db) flushPresetMirrorView(next.db, next, [key], true)
                })
            })
        }
    })
    view = next
    signal.view++
}

/**
 * Keeps a view of the selected preset while preset settings show and another preset is applied,
 * and writes and drops it once either stops.
 */
function refresh(): void {
    untrack(() => {
        const db = DBState.db
        const selection = db?.botPresets?.[db.botPresetsId]
        const candidate = selection?.id && selection.id !== peekEffectivePresetId(db) ? selection.id : null
        if (candidate && !watchingSettingsWindow) {
            watchingSettingsWindow = true
            let subscribed = false
            settingsWindowStore().subscribe(() => {
                if (subscribed) refresh()
            })
            subscribed = true
        }
        const wanted = candidate && applyEditor && isPresetEditorOpen() ? candidate : null
        if (view && (view.db !== db || view.presetId !== wanted)) closeView()
        if (!wanted) return
        if (view) derivePresetMirrorView(db, view, applyEditor!)
        else openView(db, wanted, applyEditor!)
    })
}

/** Shows preset settings outside the settings window, such as quick settings, until the returned function runs. */
export function holdPresetEditor(): () => void {
    untrack(() => signal.surfaces++)
    refresh()
    let held = true
    return () => {
        if (!held) return
        held = false
        untrack(() => signal.surfaces--)
        refresh()
    }
}

/** For a component's init: while `shown()` holds, the component edits the selected preset, as the settings window does. */
export function usePresetEditorSurface(shown: () => boolean): void {
    markPresetEditorSurface()
    $effect.pre(() => {
        if (!shown()) return
        const release = holdPresetEditor()
        // A teardown reads state from before the pending update, so the view is written once it has run.
        return () => queueMicrotask(release)
    })
}

/** Writes pending settings edits of a preset that is not applied to its record. */
export function flushPresetEditorEdits(db: Database = DBState.db): void {
    if (view && view.db === db) untrack(() => flushPresetMirrorView(db, view!))
}

/**
 * Follows a derive of the preset mirrors: publishes the applied preset and, while preset settings
 * show, gives them a view of the selected preset whenever another preset is applied.
 */
export function syncPresetEditorTarget(db: Database, apply: ApplyPreset): void {
    applyEditor = apply
    const appliedId = peekEffectivePresetId(db)
    if (applied?.db !== db || applied.id !== appliedId) {
        applied = { db, id: appliedId }
        signal.applied++
    }
    if (db === DBState.db) refresh()
}

function editingView(root: Database): EditorView | null {
    void signal.view
    return view && view.db === root ? view : null
}

function routedView(root: Database, key: string): EditorView | null {
    if (!Object.hasOwn(presetMirrorMap, key)) return null
    const current = editingView(root)
    if (!current) return null
    const flag = protectedPresetGroups[key as ProtectedPresetField]
    return flag && (root as unknown as Fields)[flag] ? null : current
}

function protectedFields(flag: string): string[] {
    return Object.entries(protectedPresetGroups).filter(([, value]) => value === flag).map(([field]) => field)
}

/**
 * A protection flag set from the settings window keeps the values the window showed:
 * turning it on shares the edited preset's values, turning it off gives the shared
 * values to the edited preset and lets the applied preset show its own again.
 */
function setProtectionFlag(root: Database, current: EditorView, flag: string, value: unknown): void {
    const fields = root as unknown as Fields
    flushEffectivePresetEdits(root)
    if (value) {
        for (const field of protectedFields(flag)) fields[field] = clone(current.values[field])
        fields[flag] = value
        flushEffectivePresetEdits(root)
        return
    }
    for (const field of protectedFields(flag)) current.values[field] = clone(fields[field])
    fields[flag] = value
    if (applyEditor) deriveEffectivePresetMirrors(root, applyEditor)
}

function editorDatabase(root: Database): Database {
    let editor = editors.get(root)
    if (editor) return editor
    editor = new Proxy(root, {
        get(target, key) {
            const routed = typeof key === 'string' ? routedView(target, key) : null
            return routed ? routed.values[key as string] : Reflect.get(target, key)
        },
        set(target, key, value) {
            if (typeof key === 'string') {
                const routed = routedView(target, key)
                if (routed) {
                    routed.values[key] = value
                    return true
                }
                const current = Object.values(protectedPresetGroups).includes(key as never) ? editingView(target) : null
                if (current && Boolean(value) !== Boolean((target as unknown as Fields)[key])) {
                    setProtectionFlag(target, current, key, value)
                    return true
                }
            }
            return Reflect.set(target, key, value)
        },
        deleteProperty(target, key) {
            const routed = typeof key === 'string' ? routedView(target, key) : null
            if (routed) {
                delete routed.values[key as string]
                return true
            }
            return Reflect.deleteProperty(target, key)
        },
    })
    editors.set(root, editor)
    return editor
}

/**
 * `DBState` for preset settings surfaces. While preset settings show, mirrored preset fields read
 * and write the selected preset (`botPresetsId`); elsewhere, and for every other field, it is the
 * live database.
 */
export const presetEditorDBState = {
    get db(): Database {
        return editorDatabase(DBState.db)
    },
}

/** The preset the mirrors hold: the chat's binding, then the chain pick, then the selection. */
export function getAppliedPresetId(db: Database = DBState.db): string | undefined {
    void signal.applied
    return applied?.db === db ? applied.id : peekEffectivePresetId(db)
}

export function getAppliedPresetIndex(db: Database = DBState.db): number {
    const id = getAppliedPresetId(db)
    const index = id ? db.botPresets?.findIndex(preset => preset?.id === id) ?? -1 : -1
    return index < 0 ? db.botPresetsId : index
}
