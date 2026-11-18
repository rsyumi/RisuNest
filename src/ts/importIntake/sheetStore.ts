import { get, writable } from 'svelte/store'
import type { NativeFileOperationImportLocation } from '../storage/nativeFileJobManager'
import type { ImportSheetDecision, ImportSheetRequest } from './types'

export interface ActiveImportSheet {
    request: ImportSheetRequest
    resolve(decision: ImportSheetDecision): void
}

export const importSheet = writable<ActiveImportSheet | null>(null)

/** True while a file drag that the intake accepts is over the window. */
export const importDropOverlayVisible = writable(false)

/** The last imported character the result's Open button went to. Views kept in a component's own state step aside for it. */
export const openedImportResult = writable<{ kind: Extract<NativeFileOperationImportLocation, { chaId: string }>['kind'] } | null>(null)

/** Shows the import sheet and resolves with the user's decision. A sheet already open is closed as cancelled. */
export function openImportSheet(request: ImportSheetRequest): Promise<ImportSheetDecision> {
    get(importSheet)?.resolve(null)
    return new Promise(resolve => {
        const active: ActiveImportSheet = {
            request,
            resolve(decision) {
                if (get(importSheet) === active) importSheet.set(null)
                resolve(decision)
            },
        }
        importSheet.set(active)
    })
}

/** Closes the open sheet with a decision; no effect when none is open. */
export function resolveImportSheet(decision: ImportSheetDecision): void {
    get(importSheet)?.resolve(decision)
}
