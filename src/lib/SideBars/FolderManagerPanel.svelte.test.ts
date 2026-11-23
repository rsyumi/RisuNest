import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, tick, unmount } from 'svelte'
import { language } from 'src/lang'
import type { folder } from 'src/ts/storage/database.svelte'
import { DBState } from 'src/ts/stores.svelte'
import { closeFolderManager, folderManager, openFolderManager, saveFolderManager } from 'src/ts/gui/folderManager.svelte'
import FolderManagerPanel from './FolderManagerPanel.svelte'

vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: { characterOrder: [], roundIcons: false } as never })
    return { DBState }
})
// Opening and saving the panel reaches none of these; importing them would start the app's store effects.
vi.mock('src/ts/globalApi.svelte', () => ({ checkCharOrder: () => {}, getFileSrc: async () => '' }))
vi.mock('src/ts/alert', () => ({ alertActionConfirm: async () => false, alertError: vi.fn() }))
vi.mock('src/ts/util', () => ({ getCharacterIndexObject: () => ({}), selectSingleFile: vi.fn() }))
vi.mock('src/ts/characters', () => ({ getCharImage: async () => '' }))
vi.mock('src/ts/storage/database.svelte', () => ({ saveImage: vi.fn() }))
vi.mock('src/ts/ui/modalNavigation', () => ({ modalNavigation: () => ({ destroy() {} }) }))
vi.mock('svelte-awesome-color-picker', () => ({ default: () => {} }))
vi.mock('./SidebarAvatar.svelte', () => ({ default: () => {} }))

const copy = language.risuNest.folderManager
let instance: ReturnType<typeof mount> | undefined

afterEach(async () => {
    closeFolderManager()
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

const label = (name: string) => [...document.querySelectorAll('span')].some((span) => span.textContent === name)
const hint = () => document.querySelector('.hint')?.textContent ?? null

it('offers the folder glyph an icon colour of its own and saves it as the folder text colour', async () => {
    const stored: folder = { name: '판타지', data: [], color: 'blue', id: 'f1', iconType: 'glyph' }
    DBState.db.characterOrder = [stored]
    openFolderManager('f1', { top: 0, right: 0 })
    instance = mount(FolderManagerPanel, { target: document.body })
    await tick()
    expect(label(copy.color)).toBe(true)
    expect(label(copy.iconColor)).toBe(true)
    expect(label(copy.textColor)).toBe(false)

    const draft = folderManager.draft!
    draft.colorMode = 'solid'
    draft.solid = '#101010'
    document.querySelector<HTMLButtonElement>(`[aria-label="${copy.iconColor}"] button:last-child`)!.click()
    flushSync()
    draft.textColor = '#111111'
    await tick()
    expect(document.querySelector(`button[aria-label="${copy.iconColor}"]`)).not.toBeNull()
    expect(hint()).toBe(copy.iconLowContrast)

    // The text icon keeps its own wording for the same colour.
    draft.iconType = 'text'
    await tick()
    expect(label(copy.textColor)).toBe(true)
    expect(hint()).toBe(copy.lowContrast)
    draft.iconType = 'glyph'
    await tick()

    await saveFolderManager()
    expect(DBState.db.characterOrder[0]).toMatchObject({ iconType: 'glyph', textColor: '#111111', colorStops: ['#101010'] })
})
