import { writable } from 'svelte/store'
import type { Database } from '../../src/ts/storage/database.svelte'
export { languageEnglish as language } from '../../src/lang/en'

export const DBState = { db: { theme: '', customBackground: '' } as unknown as Database }
export const selectedCharID = writable(-1)
export const CharEmotion = writable({})
export const getCustomBackground = async () => ''
export const getEmotion = () => ''
export const backNavigationLayer = () => ({ destroy() {} })
