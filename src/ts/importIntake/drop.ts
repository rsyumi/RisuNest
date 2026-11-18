import { language } from 'src/lang'
import { alertNormal } from '../alert'
import { importCharacterProcess } from '../characterCards'
import { RISU_APP_INTERNAL_DRAG_TYPE, RISU_SIDEBAR_DRAG_TYPE } from '../dragTypes'
import { checkCharOrder } from '../globalApi.svelte'
import { isTauri } from '../platform'
import { readModule } from '../process/modules'
import { importPreset } from '../storage/database.svelte'
import { DBState } from '../stores.svelte'

// Contract state: the App.svelte drop handler, moved here unchanged. The intake worker replaces it.

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

export function handleAppDragOver(e: DragEvent): void {
    const dropEffect = getMainDropEffect(e)
    e.preventDefault()
    e.dataTransfer.dropEffect = dropEffect
}

export function handleAppDragStart(e: DragEvent): void {
    e.dataTransfer?.setData(RISU_APP_INTERNAL_DRAG_TYPE, 'true')
}

export async function handleAppDrop(e: DragEvent): Promise<void> {
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
        const data = new Uint8Array(await file.arrayBuffer())
        await importPreset({ name: file.name, data })
        alertNormal(language.successImport)
    } else if (name.endsWith('.risum')) {
        if (isTauri) {
            const { importNativeContentFile } = await import('../storage/nativeContentFile')
            await importNativeContentFile(file, 'module')
            return
        }
        const data = new Uint8Array(await file.arrayBuffer())
        const module = await readModule(Buffer.from(data))
        DBState.db.modules.push(module)
        alertNormal(language.successImport)
    } else {
        await importCharacterProcess({
            name: file.name,
            data: file
        })
        checkCharOrder()
    }
}
