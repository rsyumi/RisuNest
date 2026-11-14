import '../../src/styles.css'
import { mount } from 'svelte'
import DragDrop from './DragDrop.svelte'
import { DBState } from './dragDropAdapters.svelte'

mount(DragDrop, { target: document.body, props: { kind: new URLSearchParams(location.search).get('kind') ?? 'lore' } })
Object.assign(window, { dragState: DBState })
