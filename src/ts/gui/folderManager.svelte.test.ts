import { describe, expect, it, vi } from 'vitest'
import type { folder } from '../storage/database.svelte'
import { DBState } from '../stores.svelte'
import { folderManager, openFolderManager, saveFolderManager } from './folderManager.svelte'

const fileSource = vi.hoisted(() => ({ resolve: (_source: string) => {} }))

vi.mock('../globalApi.svelte', () => ({
    checkCharOrder: () => {},
    getFileSrc: () => new Promise<string>((resolve) => { fileSource.resolve = resolve }),
}))
// Saving reaches none of these; importing them would start the app's store effects and the parser.
vi.mock('../stores.svelte', () => ({ DBState: { db: {} } }))
vi.mock('../alert', () => ({ alertActionConfirm: async () => false }))
vi.mock('../util', () => ({ getCharacterIndexObject: () => ({}) }))

describe('saveFolderManager', () => {
    it('saves onto the folder as it is after a new image resolves', async () => {
        const original: folder = { name: '판타지', data: ['a'], color: 'blue', id: 'f1' }
        DBState.db.characterOrder = ['x', original]
        openFolderManager('f1', { top: 0, right: 0 })
        folderManager.draft!.name = '로맨스'
        folderManager.draft!.imgFile = 'assets/new.png'
        folderManager.draft!.iconType = 'image'
        const saving = saveFolderManager()
        // Another device moves the folder and adds a character while the image source resolves.
        DBState.db.characterOrder = [{ ...original, data: ['a', 'b'] }, 'x']
        fileSource.resolve('blob:new')
        await saving
        expect(DBState.db.characterOrder).toEqual([{ ...original, name: '로맨스', data: ['a', 'b'], imgFile: 'assets/new.png', img: 'blob:new' }, 'x'])
    })
})
