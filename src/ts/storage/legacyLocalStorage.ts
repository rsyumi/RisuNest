import localforage from 'localforage'
import { isNodeServer } from '../platform'
import { NodeStorage } from './nodeStorage'
import { OpfsStorage } from './opfsStorage'

export interface LegacyLocalStorage {
    getItem(key: string): Promise<Uint8Array | null>
    keys(): Promise<string[]>
}

export interface LegacyLocalStorageSelection {
    isNodeServer: boolean
    canUseOpfs: boolean
    opfsEnabled: boolean
    createNodeStorage(): LegacyLocalStorage
    createOpfsStorage(): LegacyLocalStorage
    createForageStorage(): LegacyLocalStorage
}

export function selectLegacyLocalStorage(
    selection: LegacyLocalStorageSelection,
): LegacyLocalStorage {
    if (selection.isNodeServer) return selection.createNodeStorage()
    if (selection.canUseOpfs && selection.opfsEnabled) {
        return selection.createOpfsStorage()
    }
    return selection.createForageStorage()
}

export function getLegacyLocalStorage(): LegacyLocalStorage {
    const canUseOpfs =
        typeof window !== 'undefined' &&
        Boolean(window.navigator?.storage?.getDirectory) &&
        typeof FileSystemFileHandle !== 'undefined' &&
        Boolean(FileSystemFileHandle.prototype.createWritable)
    return selectLegacyLocalStorage({
        isNodeServer,
        canUseOpfs,
        opfsEnabled: localStorage.getItem('opfs_flag!') === 'able',
        createNodeStorage: () => new NodeStorage(),
        createOpfsStorage: () => new OpfsStorage(),
        createForageStorage: () => localforage.createInstance({ name: 'risuai' }) as LegacyLocalStorage,
    })
}
