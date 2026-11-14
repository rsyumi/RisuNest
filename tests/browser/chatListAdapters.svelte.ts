import { writable } from 'svelte/store'
import type { character, Database } from '../../src/ts/storage/database.svelte'
export { languageEnglish as language } from '../../src/lang/en'
export { sortableOptions } from '../../src/ts/sortableOptions'

const chatCount = Number(new URLSearchParams(location.search).get('chats') ?? 3)
export const DBState = $state({ db: { characters: [{
    chaId: 'synthetic', name: 'Synthetic', chatPage: 0,
    chats: Array.from({ length: chatCount }, (_, i) => ({ id: `chat-${i + 1}`, name: `Chat ${i + 1}`, message: [], note: '', localLore: [] })),
    chatFolders: [{ id: 'first', name: 'First folder', folded: false }, { id: 'second', name: 'Second folder', folded: false }],
}] } as unknown as Database })
export const selectedCharID = writable(0)
export const bookmarkListOpen = writable(false)
export const isWorkingSetCharacterStub = () => false
// The app saves the edited list; the fixture only has to show it.
export const editSelectedChatList = async (_characterId: string, _reason: string, edit: (character: character) => string | null | false) =>
    edit(DBState.db.characters[0] as character) !== false
export const changeChatTo = async (id: string) => {
    const selected = DBState.db.characters[0] as character
    selected.chatPage = selected.chats.findIndex(chat => chat.id === id)
}
