import { mount } from 'svelte'
import ChatScreen from '../../src/lib/ChatScreens/ChatScreen.svelte'

// As in the desktop layout, the sidebar panel is an unpositioned block beside the chat pane.
const main = document.createElement('main')
main.style.cssText = 'display: flex; width: 100%; height: 100%'
const sidebar = document.createElement('div')
sidebar.dataset.testid = 'sidebar'
sidebar.style.cssText = 'width: 464px; flex-shrink: 0; height: 100%; background: rgb(255, 0, 255)'
main.append(sidebar)
document.body.append(main)
mount(ChatScreen, { target: main })
