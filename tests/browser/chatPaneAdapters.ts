import { writable } from 'svelte/store'
import type { Database } from '../../src/ts/storage/database.svelte'
export { languageEnglish as language } from '../../src/lang/en'

export const DBState = { db: { theme: '', customBackground: '', characters: [{ viewScreen: 'none' }] } as unknown as Database }
// `?home` stands for the home screen, where no character is selected.
export const selectedCharID = writable(new URLSearchParams(location.search).has('home') ? -1 : 0)
export const CharEmotion = writable({})
export const getCustomBackground = async () => ''
export const getEmotion = () => ''
export const backNavigationLayer = () => ({ destroy() {} })
