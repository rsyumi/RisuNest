import { get } from 'svelte/store'
import { onboardingHold } from 'src/lib/Others/Onboarding/onboardingGate'
import { language } from 'src/lang'
import { alertNormal } from '../alert'
import { isAppLockInputBlocked } from '../appLock/input'
import { RISU_APP_INTERNAL_DRAG_TYPE, RISU_SIDEBAR_DRAG_TYPE } from '../dragTypes'
import { isTauri } from '../platform'
import { persistentWorkingSetInputBlocked } from '../storage/persistentDataRuntime.svelte'
import { DBState, loadedStore } from '../stores.svelte'
import { importDropOverlayVisible } from './sheetStore'
import type { ImportIntakeDelivery } from './types'

export const getMainDropEffect = (e: DragEvent): DataTransfer['dropEffect'] => {
    const types = Array.from(e.dataTransfer?.types ?? [])
    if (types.includes(RISU_SIDEBAR_DRAG_TYPE)) {
        return 'none'
    }
    if (types.includes(RISU_APP_INTERNAL_DRAG_TYPE)) {
        return 'none'
    }
    return types.includes('Files') ? 'copy' : 'none'
}

/** A drag that carries files from outside the app. */
function isFileDrag(e: DragEvent): boolean {
    return getMainDropEffect(e) === 'copy'
}

/** Drops are refused while onboarding is on screen or the app lock covers the app. */
export function importDropsAccepted(): boolean {
    return get(loadedStore)
        && !!DBState.db?.didFirstSetup
        && !get(onboardingHold)
        && !isAppLockInputBlocked()
        && !get(persistentWorkingSetInputBlocked)
}

export function handleAppDragOver(e: DragEvent): void {
    if (isTauri) {
        // The document listeners own file drags; this only keeps the effect they chose.
        e.preventDefault()
        if (e.dataTransfer) e.dataTransfer.dropEffect = isFileDrag(e) && importDropsAccepted() ? 'copy' : 'none'
        return
    }
    const dropEffect = getMainDropEffect(e)
    e.preventDefault()
    e.dataTransfer.dropEffect = dropEffect
}

export function handleAppDragStart(e: DragEvent): void {
    e.dataTransfer?.setData(RISU_APP_INTERNAL_DRAG_TYPE, 'true')
}

export async function handleAppDrop(e: DragEvent): Promise<void> {
    if (isTauri) {
        e.preventDefault()
        return
    }
    const types = Array.from(e.dataTransfer.types ?? [])
    if (types.includes(RISU_APP_INTERNAL_DRAG_TYPE) || types.includes(RISU_SIDEBAR_DRAG_TYPE)) {
        e.preventDefault()
        return
    }
    const file = e.dataTransfer.files[0]
    if (!file) {
        e.preventDefault()
        return
    }
    e.preventDefault()
    const name = file.name.toLowerCase()

    if (name.endsWith('.risup')) {
        const { importPreset } = await import('../storage/database.svelte')
        const data = new Uint8Array(await file.arrayBuffer())
        await importPreset({ name: file.name, data })
        alertNormal(language.successImport)
    } else if (name.endsWith('.risum')) {
        const { readModule } = await import('../process/modules')
        const data = new Uint8Array(await file.arrayBuffer())
        const module = await readModule(Buffer.from(data))
        DBState.db.modules.push(module)
        alertNormal(language.successImport)
    } else {
        const [{ importCharacterProcess }, { checkCharOrder }] = await Promise.all([
            import('../characterCards'),
            import('../globalApi.svelte'),
        ])
        await importCharacterProcess({
            name: file.name,
            data: file
        })
        checkCharOrder()
    }
}

/**
 * Watches file drags on the whole document: shows the drop overlay while an accepted drag is over
 * the window, keeps the WebView from opening a dropped file, and hands each accepted drop to
 * `submit` as one delivery in drop order. Returns the disposer.
 */
export function installImportDropListeners(submit: (delivery: ImportIntakeDelivery) => void): () => void {
    // Elements the drag is over. One removed during the drag gets its dragleave while detached, which
    // never reaches the document, so it is dropped once it is no longer connected.
    const entered = new Set<EventTarget>()
    const dragInside = () => {
        for (const target of entered) if (target instanceof Node && !target.isConnected) entered.delete(target)
        return entered.size > 0
    }
    const hide = () => {
        entered.clear()
        importDropOverlayVisible.set(false)
    }
    const onDragEnter = (e: DragEvent) => {
        if (!isFileDrag(e)) return
        e.preventDefault()
        if (e.target) entered.add(e.target)
        importDropOverlayVisible.set(importDropsAccepted())
    }
    const onDragOver = (e: DragEvent) => {
        if (!isFileDrag(e)) return
        e.preventDefault()
        const accepted = importDropsAccepted()
        if (e.dataTransfer) e.dataTransfer.dropEffect = accepted ? 'copy' : 'none'
        importDropOverlayVisible.set(accepted && dragInside())
    }
    const onDragLeave = (e: DragEvent) => {
        if (!isFileDrag(e)) return
        if (e.target) entered.delete(e.target)
        if (!dragInside()) importDropOverlayVisible.set(false)
    }
    const onDrop = (e: DragEvent) => {
        if (!isFileDrag(e)) return
        e.preventDefault()
        hide()
        if (!importDropsAccepted()) return
        // The list is only readable during the event, so the files are taken now.
        const files = Array.from(e.dataTransfer?.files ?? [])
        if (files.length) submit({ origin: 'drop', sources: files.map(file => ({ type: 'file', file })) })
    }
    const onDragEnd = () => hide()
    const listeners = [
        ['dragenter', onDragEnter],
        ['dragover', onDragOver],
        ['dragleave', onDragLeave],
        ['drop', onDrop],
        ['dragend', onDragEnd],
    ] as const
    // Capture, so a nested handler that stops a file drag cannot let the WebView open the file.
    for (const [type, listener] of listeners) document.addEventListener(type, listener, true)
    return () => {
        for (const [type, listener] of listeners) document.removeEventListener(type, listener, true)
        hide()
    }
}
