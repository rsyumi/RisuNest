// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'

vi.mock('../../ts/util', () => ({
    getCustomBackground: vi.fn(async () => ''),
    getEmotion: vi.fn(() => ''),
}))

vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        db: {
            theme: 'classic',
            customBackground: '',
            textScreenColor: '',
            textBorder: false,
            textScreenRounded: false,
            textScreenBorder: '',
            classicMaxWidth: false,
            characters: [
                {
                    chaId: 'synthetic',
                    chatPage: 0,
                    viewScreen: 'none',
                    chats: [{ id: 'chat', message: [] }],
                },
            ],
        },
    },
    CharEmotion: writable('plain'),
    selectedCharID: writable(0),
}))

vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({
        captureSelectedConversationTarget: () => null,
        subscribeActiveConversationViewportSource: () => () => {},
    }),
}))

vi.mock('../../lang', () => ({
    language: { loadingChatData: 'Loading chat data' },
}))
vi.mock('./ResizeBox.svelte', () => ({ default: () => {} }))
vi.mock('./DefaultChatScreen.svelte', async () => ({
    default: (await import('./test-fixtures/NavigationChatContentStub.svelte'))
        .default,
}))
vi.mock('../Others/ChatList.svelte', async () => ({
    default: (await import('./test-fixtures/NavigationChatListStub.svelte'))
        .default,
}))
vi.mock('./TransitionImage.svelte', () => ({ default: () => {} }))
vi.mock('./BackgroundDom.svelte', () => ({ default: () => {} }))
vi.mock('../UI/GUI/SideBarArrow.svelte', () => ({ default: () => {} }))
vi.mock('../Setting/Pages/Module/ModuleChatMenu.svelte', () => ({
    default: () => {},
}))

import ChatScreen from './ChatScreen.svelte'
import { navigationActivity } from '../../ts/ui/navigationActivity'

let mounted: ReturnType<typeof mount> | undefined

afterEach(async () => {
    navigationActivity.set(null)
    if (mounted) await unmount(mounted)
    mounted = undefined
    document.body.replaceChildren()
})

describe('ChatScreen navigation activity', () => {
    it('marks the pane busy and makes the old chat content inert while loading', async () => {
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(ChatScreen, { target })

        const oldChatControl = target.querySelector<HTMLButtonElement>(
            '[data-old-chat-control]',
        )
        oldChatControl?.click()
        await tick()

        navigationActivity.set({ token: 1, kind: 'conversation' })
        await tick()

        const pane = target.firstElementChild as HTMLElement
        const content = pane.firstElementChild as HTMLElement
        expect(pane.getAttribute('aria-busy')).toBe('true')
        expect(content.hasAttribute('inert')).toBe(true)
        expect(
            target.querySelector('[role="status"]')?.textContent?.trim(),
        ).toBe('Loading chat data')
        expect(oldChatControl?.closest('[inert]')).toBe(content)
        const chatListControl = target.querySelector('[data-chat-list-control]')
        expect(chatListControl?.closest('[inert]')).toBeNull()
        expect(
            chatListControl?.parentElement?.classList.contains('absolute'),
        ).toBe(true)
        expect(
            chatListControl?.parentElement?.classList.contains('inset-0'),
        ).toBe(true)
        expect(chatListControl?.parentElement?.classList.contains('z-40')).toBe(
            true,
        )
    })
})
