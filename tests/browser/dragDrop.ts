import '../../src/styles.css'
import { mount, unmount } from 'svelte'
import DragDrop from './DragDrop.svelte'
import { DBState } from './dragDropAdapters.svelte'

const fixture = mount(DragDrop, { target: document.body, props: { kind: new URLSearchParams(location.search).get('kind') ?? 'lore' } })
Object.assign(window, { dragState: DBState, closeDragFixture: () => unmount(fixture) })
