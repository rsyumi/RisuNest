import '../../src/styles.css'
import { mount } from 'svelte'
import SideChatList from '../../src/lib/SideBars/SideChatList.svelte'
import { DBState } from './chatListAdapters.svelte'
import type { character } from '../../src/ts/storage/database.svelte'

document.body.style.cssText = 'margin:0;padding:24px 16px;width:300px'
// The app refuses its own drags wherever a list does not take them.
document.addEventListener('dragover', (event) => {
    event.preventDefault()
    if (event.dataTransfer) event.dataTransfer.dropEffect = 'none'
})
mount(SideChatList, { target: document.body, props: { chara: DBState.db.characters[0] as character } })
Object.assign(window, { chatListState: DBState })
