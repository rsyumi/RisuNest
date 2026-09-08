import type { character, Message } from '../../src/ts/storage/database.svelte'

export const FIXTURE_SCHEMA_VERSION = 1
export const FIXTURE_CHARACTER_COUNT = 2
export const FIXTURE_MESSAGE_COUNT = 180

const prose = [
    'A bounded synthetic paragraph exercises emphasis, **strong text**, `inline code`, and [a local anchor](#fixture).',
    '> A deterministic quote keeps parsing work stable across samples.',
    '',
    '| column | value |',
    '| --- | ---: |',
    '| alpha | 13 |',
    '| beta | 21 |',
    '',
    '```typescript',
    'const values = Array.from({ length: 24 }, (_, index) => index * 3)',
    'const total = values.reduce((sum, value) => sum + value, 0)',
    '```',
].join('\n')

function createMessages(characterIndex: number): Message[] {
    return Array.from({ length: FIXTURE_MESSAGE_COUNT }, (_, messageIndex) => ({
        role: messageIndex % 2 === 0 ? 'user' : 'char',
        data: [
            `UIBENCH_RENDERED_${characterIndex}_${messageIndex}`,
            `Synthetic message ${messageIndex + 1}.`,
            prose,
        ].join('\n\n'),
        chatId: `synthetic-${characterIndex}-${messageIndex}`,
        time: 1_700_000_000_000 + messageIndex,
    }))
}

export function createFixtureCharacters(): character[] {
    return Array.from(
        { length: FIXTURE_CHARACTER_COUNT },
        (_, characterIndex) => {
            const messages = createMessages(characterIndex)
            return {
                type: 'character',
                name: `Synthetic Character ${characterIndex + 1}`,
                image: '',
                firstMessage: `UIBENCH_START_${characterIndex}`,
                desc: 'Local synthetic benchmark character.',
                personality: '',
                scenario: '',
                exampleMessage: '',
                chats: [
                    {
                        id: `synthetic-conversation-${characterIndex}`,
                        name: 'Synthetic conversation',
                        note: '',
                        localLore: [],
                        message: messages,
                        fmIndex: -1,
                        bookmarks: [],
                    },
                ],
                chatFolders: [],
                chatPage: 0,
                chaId: `synthetic-character-${characterIndex}`,
                customscript: [],
                triggerscript: [],
                emotionImages: [],
                additionalAssets: [],
                alternateGreetings: [],
                creatorNotes: '',
                removedQuotes: false,
                viewScreen: 'none',
                utilityBot: false,
                replaceGlobalNote: '',
                additionalText: '',
            } as character
        },
    )
}

export function fixtureCanonicalText(): string {
    return JSON.stringify(
        createFixtureCharacters().map((character) => ({
            id: character.chaId,
            firstMessage: character.firstMessage,
            messages: character.chats[0].message.map((message) => ({
                role: message.role,
                data: message.data,
                chatId: message.chatId,
                time: message.time,
            })),
        })),
    )
}
