import '../../src/styles.css'
import { mount } from 'svelte'
import SideChatList from '../../src/lib/SideBars/SideChatList.svelte'
import { DBState } from './chatListAdapters.svelte'
import type { character } from '../../src/ts/storage/database.svelte'

document.body.style.cssText = 'margin:0;padding:24px 16px;width:300px'
mount(SideChatList, { target: document.body, props: { chara: DBState.db.characters[0] as character } })
Object.assign(window, { chatListState: DBState })
