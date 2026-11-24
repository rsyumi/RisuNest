import { mount } from 'svelte'
import ChatScreen from '../../src/lib/ChatScreens/ChatScreen.svelte'

// As in the desktop layout, the sidebar panel is an unpositioned block beside the chat pane. Its text fields are
// positioned at z-index 20, and the save icon after the main element is positioned at z-index 10.
const main = document.createElement('main')
main.style.cssText = 'display: flex; width: 100%; height: 100%'
const sidebar = document.createElement('div')
sidebar.dataset.testid = 'sidebar'
sidebar.style.cssText = 'width: 464px; flex-shrink: 0; height: 100%; background: rgb(255, 0, 255)'
const field = document.createElement('div')
field.dataset.testid = 'sidebar-field'
field.style.cssText = 'position: relative; z-index: 20; top: 300px; left: 20px; width: 200px; height: 60px; background: rgb(0, 255, 255)'
sidebar.append(field)
main.append(sidebar)
const saveIcon = document.createElement('div')
saveIcon.dataset.testid = 'save-icon'
saveIcon.style.cssText = 'position: absolute; z-index: 10; top: 12px; right: 12px; width: 40px; height: 40px; background: rgb(255, 128, 0)'
document.body.append(main, saveIcon)
mount(ChatScreen, { target: main })
