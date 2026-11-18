import { untrack } from 'svelte'
import { derived, fromStore } from 'svelte/store'
import { DBState, MobileGUI, MobileGUIStack, MobileSideBar, selectedCharID, settingsOpen } from '../stores.svelte'
import type { Database } from './database.svelte'
import { safeStructuredClone } from '../safeStructuredClone'
import {
    createPresetMirrorValues,
    deriveEffectivePresetMirrors,
    derivePresetMirrorView,
    flushEffectivePresetEdits,
    flushPresetMirrorView,
    isLoadedPreset,
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
const signal = $state({ view: 0, applied: 0 })
let view: EditorView | null = null
let applied: { db: Database; id: string | undefined } | null = null
let settingsWindow: { readonly current: boolean } | undefined
let applyForFlags: ApplyPreset | undefined
const editors = new WeakMap<Database, Database>()

const clone = <T>(value: T): T => value === undefined ? value : safeStructuredClone(value)

// The settings window, or the mobile screen that shows settings when no character is selected.
function settingsWindowOpen(): boolean {
    settingsWindow ??= fromStore(derived(
        [settingsOpen, MobileGUI, MobileGUIStack, MobileSideBar, selectedCharID],
        ([open, mobile, screen, sideBar, character]) => open || (mobile && screen === 2 && sideBar === 0 && character === -1),
    ))
    return settingsWindow.current
}

function track(value: unknown): void {
    if (!value || typeof value !== 'object') return
    for (const key in value as Fields) track((value as Fields)[key])
}

function closeView(): void {
    if (!view) return
    if (view.db === DBState.db) flushPresetMirrorView(view.db, view)
    view.dispose()
    view = null
    signal.view++
}

function openView(db: Database, presetId: string, apply: ApplyPreset): void {
    const initial = createPresetMirrorValues(db, presetId, apply)
    if (!initial) return
    const snapshot = Object.fromEntries(Object.entries(initial).map(([key, value]) => [key, clone(value)]))
    const values = $state(initial)
    const next: EditorView = { db, presetId, values, snapshot, dispose: () => {} }
    // Edits reach the record as they happen, which the save observer then picks up.
    next.dispose = $effect.root(() => {
        for (const key of Object.keys(presetMirrorMap)) {
            $effect(() => {
                track(values[key])
                untrack(() => {
                    if (view === next && next.db === DBState.db) flushPresetMirrorView(next.db, next, [key])
                })
            })
        }
    })
    view = next
    signal.view++
}

/** Writes pending settings edits of a preset that is not applied to its record. */
export function flushPresetEditorEdits(db: Database = DBState.db): void {
    if (view && view.db === db) flushPresetMirrorView(db, view)
}

/**
 * Follows a derive of the preset mirrors: publishes the applied preset and gives the
 * settings window a view of the selected preset whenever another preset is applied.
 */
export function syncPresetEditorTarget(db: Database, apply: ApplyPreset): void {
    applyForFlags = apply
    const appliedId = peekEffectivePresetId(db)
    if (applied?.db !== db || applied.id !== appliedId) {
        applied = { db, id: appliedId }
        signal.applied++
    }
    if (db !== DBState.db) return
    const selection = db.botPresets?.[db.botPresetsId]
    const wanted = selection?.id && selection.id !== appliedId && isLoadedPreset(selection) ? selection.id : null
    if (view && (view.db !== db || view.presetId !== wanted)) closeView()
    if (!wanted) return
    if (view) derivePresetMirrorView(db, view, apply)
    else openView(db, wanted, apply)
}

function editingView(root: Database): EditorView | null {
    void signal.view
    const current = view
    if (!current || current.db !== root) return null
    return settingsWindowOpen() ? current : null
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
    if (applyForFlags) deriveEffectivePresetMirrors(root, applyForFlags)
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
 * `DBState` for preset settings surfaces. While the settings window is open, mirrored preset
 * fields read and write the selected preset (`botPresetsId`); elsewhere, and for every other
 * field, it is the live database.
 */
export const presetEditorDBState = {
    get db(): Database {
        return editorDatabase(DBState.db)
    },
}

export function getSelectedPresetIndex(db: Database = DBState.db): number {
    return db.botPresetsId
}

export function getSelectedPresetId(db: Database = DBState.db): string | undefined {
    return db.botPresets?.[db.botPresetsId]?.id
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

/** The preset `presetEditorDBState` edits where it is read: the selection in the settings window, the applied preset elsewhere. */
export function getEditedPresetId(db: Database = DBState.db): string | undefined {
    return settingsWindowOpen() ? getSelectedPresetId(db) : getAppliedPresetId(db)
}
