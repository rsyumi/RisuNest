import { writable } from 'svelte/store'
import type { Database } from '../../src/ts/storage/database.svelte'
import { indexCharactersById } from '../../src/ts/characterIndex'
export { languageEnglish as language } from '../../src/lang/en'

export const DBState = $state({ db: {
    characters: Array.from({ length: 20 }, (_, i) => ({ chaId: `bot-${i + 1}`, name: `Bot ${i + 1}`, image: `assets/bot-${i + 1}.png`, chats: [] })),
    characterOrder: Array.from({ length: 20 }, (_, i) => `bot-${i + 1}`),
} as unknown as Database, clicks: 0, menus: 0 })
export const selectedCharID = writable(-1)
export const DynamicGUI = writable(false)
export const sideBarClosing = writable(false)
export const sideBarSize = writable(0)
export const tooltipRight = () => ({})
export const checkCharOrder = () => {}
export const getCharacterIndexObject = () => indexCharactersById(DBState.db.characters)
export const changeChar = (index: number) => { DBState.clicks++; selectedCharID.set(index) }
export const folderManager = $state({ folderId: null as string | null, anchor: null, draft: null })
export const openFolderManager = () => { DBState.menus++ }

export const getCharImage = () => ""
