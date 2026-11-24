import { resolve } from 'node:path'
import type { Plugin } from 'vite'
import { stubbed } from './autoStubPlugin'

// The real ChatScreen with its chat column replaced by card-style fixed layers, a stand-in sidebar arrow and its other screens left empty.
export function chatPaneBoundaries(): Plugin {
    return {
        name: 'chat-pane-fixture-boundaries', enforce: 'pre',
        async resolveId(source, importer, options) {
            const owner = importer?.replaceAll('\\', '/')
            if (!owner?.endsWith('/src/lib/ChatScreens/ChatScreen.svelte')) return null
            const fixture = (file: string) => resolve(import.meta.dirname, file).replaceAll('\\', '/')
            if (source.endsWith('/DefaultChatScreen.svelte')) return fixture('ChatPaneContent.svelte')
            if (source.endsWith('/SideBarArrow.svelte')) return fixture('ChatPaneArrow.svelte')
            if (source.endsWith('/ChatWindowLayer.svelte')) return null
            if (/(^|\/)(stores\.svelte|util|lang|modalNavigation)$/.test(source.replaceAll('\\', '/'))) return stubbed(this, fixture('chatPaneAdapters.ts'), source, importer!, options)
            if (source.endsWith('.svelte')) return fixture('Empty.svelte')
            return null
        },
    }
}
