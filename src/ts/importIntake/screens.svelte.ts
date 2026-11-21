import { fromStore, toStore, type Readable } from 'svelte/store'
import { showRealmInfoStore } from '../characterCards'
import { textEditorPopup } from '../gui/textEditorPopup.svelte'
import { persistentWorkingSetInputBlocked } from '../storage/persistentDataRuntime.svelte'
import { externalApplicationRecovery } from '../storage/sync/external/applicationRecovery'
import {
    customSideBarConfigDialogStore,
    irisStore,
    onboardingModalStore,
    popUpEditorStore,
    ShowRealmFrameStore,
} from '../stores.svelte'
import { appUpdateState } from '../update/state.svelte'

/** Reads a store and, inside an effect, tracks it. */
const read = <T>(store: Readable<T>): T => fromStore(store).current

/**
 * A screen the import sheet would open under: the library recovery screen, which also makes the app inert, the
 * update popup, or a full-screen editor or Realm view drawn above the sheet.
 */
export function importCoveringScreenOpen(): boolean {
    return read(persistentWorkingSetInputBlocked)
        || read(externalApplicationRecovery) !== null
        || read(appUpdateState).popupVisible
        || textEditorPopup.request !== null
        || !!read(ShowRealmFrameStore)
        || read(showRealmInfoStore) !== null
        || popUpEditorStore.open
        || irisStore.open
        || onboardingModalStore.open
        || customSideBarConfigDialogStore.open
}

/** Resolves once no covering screen is open. */
export function waitForImportCoveringScreensClosed(): Promise<void> {
    if (!importCoveringScreenOpen()) return Promise.resolve()
    return new Promise(resolve => {
        let stop: (() => void) | undefined
        let closed = false
        stop = toStore(importCoveringScreenOpen).subscribe(open => {
            if (open || closed) return
            closed = true
            resolve()
            // The first call runs inside subscribe, before `stop` is assigned.
            queueMicrotask(() => stop?.())
        })
    })
}
