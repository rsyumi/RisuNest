// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { character } from 'src/ts/storage/database.svelte'

vi.mock('./Chat.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))
vi.mock('./CreatorQuote.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))

import ChatConversationStart from './ChatConversationStart.svelte'

function metadataOnlyCharacter(): character {
    const conversation = { id: 'chat-id', fmIndex: -1 } as character['chats'][number]
    Object.defineProperty(conversation, 'message', {
        get() {
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    return {
        type: 'character',
        name: 'Character',
        chaId: 'character-id',
        chatPage: 0,
        chats: [conversation],
        firstMessage: 'Greeting',
        alternateGreetings: [],
        creatorNotes: '',
        removedQuotes: false,
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [],
    } as unknown as character
}

describe('ChatConversationStart', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        document.body.replaceChildren()
    })

    test('uses the supplied total count for the empty warning without reading the shell body', () => {
        mounted = mount(ChatConversationStart, {
            target,
            props: {
                currentCharacter: metadataOnlyCharacter(),
                resolvedImage: '',
                showAiWarning: true,
                totalMessages: 0,
                onReroll: () => {},
                unReroll: () => {},
                onRemoveCreatorQuote: () => {},
            },
        })

        expect(target.querySelector('.italic')).not.toBeNull()
    })
})
