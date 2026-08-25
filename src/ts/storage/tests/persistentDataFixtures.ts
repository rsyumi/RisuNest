import type { Chat, Database, Message, character } from '../database.svelte'

function makeMessages(count: number): Message[] {
    return Array.from({ length: count }, (_, index) => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: `message-${index.toString().padStart(3, '0')}`,
        chatId: `msg-${index.toString().padStart(3, '0')}`,
        time: 1_700_000_000_000 + index,
    }))
}

function makeConversation(id: string, name: string, messageCount: number, lastDate: number): Chat {
    return {
        id,
        name,
        note: '',
        localLore: [],
        message: makeMessages(messageCount),
        lastDate,
    }
}

function makeCharacter(input: {
    id: string
    name: string
    lastInteraction: number
    trashTime?: number
    chats: Chat[]
}): character {
    return {
        type: 'character',
        chaId: input.id,
        name: input.name,
        image: `${input.id}.png`,
        firstMessage: `Hello from ${input.name}`,
        desc: `${input.name} description`,
        notes: '',
        chats: input.chats,
        chatFolders: [],
        chatPage: 0,
        viewScreen: 'none',
        bias: [],
        emotionImages: [],
        globalLore: [],
        sdData: [],
        customscript: [],
        triggerscript: [],
        utilityBot: false,
        exampleMessage: '',
        creatorNotes: '',
        systemPrompt: '',
        postHistoryInstructions: '',
        replaceGlobalNote: '',
        additionalText: '',
        alternateGreetings: [],
        tags: [],
        creator: 'fixture',
        characterVersion: '1',
        personality: '',
        scenario: '',
        firstMsgIndex: 0,
        lastInteraction: input.lastInteraction,
        trashTime: input.trashTime,
    }
}

export const fixtureDatabase = {
    apiType: 'fixture-provider',
    username: 'Fixture User',
    formatversion: 4,
    botPresets: [
        { name: 'Preset Beta', image: 'preset-beta.png', mainPrompt: 'second' },
        { name: 'Preset Alpha', mainPrompt: 'first' },
    ],
    pluginCustomStorage: {},
    characters: [
        makeCharacter({
            id: 'char-b',
            name: 'Beta',
            lastInteraction: 200,
            chats: [makeConversation('conv-beta', 'Beta chat', 3, 200)],
        }),
        makeCharacter({
            id: 'char-a',
            name: 'Alpha',
            lastInteraction: 300,
            chats: [
                makeConversation('conv-long', 'Long chat', 130, 300),
                makeConversation('conv-short', 'Short chat', 2, 250),
            ],
        }),
        makeCharacter({
            id: 'char-c',
            name: 'Gamma',
            lastInteraction: 400,
            trashTime: 350,
            chats: [makeConversation('conv-trash', 'Trashed chat', 1, 400)],
        }),
    ],
} as unknown as Database
