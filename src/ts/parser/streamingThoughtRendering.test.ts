import { beforeEach, describe, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'

const parserMocks = vi.hoisted(() => ({
    characters: [] as unknown[],
    selIdState: { selId: -1 },
    getModuleAssets: vi.fn((): [string, string, string][] => [
        ['Theme', 'theme.mp3', 'mp3'],
    ]),
    getCurrentCharacter: vi.fn(() => ({ image: 'live.png' })),
    getFileSrc: vi.fn((path: string) => Promise.resolve(`resolved:${path}`)),
}))

vi.mock(
    import('../storage/database.svelte'),
    () =>
        ({
            appVer: '1.0.0',
            getCurrentCharacter: parserMocks.getCurrentCharacter,
            getDatabase: () => ({}),
        }) as unknown as typeof import('../storage/database.svelte'),
)

vi.mock(import('../globalApi.svelte'), () => ({
    aiWatermarkingLawApplies: () => false,
    getFileSrc: parserMocks.getFileSrc,
}))

vi.mock(
    import('../stores.svelte'),
    () =>
        ({
            DBState: {
                db: {
                    assetWidth: -1,
                    hideAllImages: false,
                    legacyMediaFindings: false,
                    assetMaxDifference: 1,
                    characters: parserMocks.characters,
                    globalChatVariables: {},
                    templateDefaultVariables: '',
                },
            },
            selIdState: parserMocks.selIdState,
            selectedCharID: writable(-1),
        }) as unknown as typeof import('../stores.svelte'),
)

vi.mock(import('../process/modules'), () => ({
    getModuleAssets: parserMocks.getModuleAssets,
    getModuleLorebooks: () => [],
    getModules: () => [],
}))

vi.mock(
    import('../process/scripts'),
    () =>
        ({
            processScriptFull: vi.fn((_char: unknown, data: string) =>
                Promise.resolve({ data, emoChanged: false }),
            ),
        }) as unknown as typeof import('../process/scripts'),
)

import { ParseMarkdown, type simpleCharacterArgument } from './parser.svelte'
import { processScriptFull } from '../process/scripts'

const character: simpleCharacterArgument = {
    type: 'simple',
    chaId: 'fixture-character',
    customscript: [],
    additionalAssets: [
        ['Portrait', 'portrait.png', 'png'],
        ['happy-face.png', 'happy.png', 'png'],
    ],
    emotionImages: [['Smile', 'smile.png']],
}

describe('streaming thought rendering through the canonical parser', () => {
    beforeEach(() => {
        vi.mocked(processScriptFull)
            .mockReset()
            .mockImplementation(async (_char, data) => ({
                data,
                emoChanged: false,
            }))
    })
    it('keeps blank lines and Markdown punctuation literal inside the recent preview', async () => {
        const recent =
            'First\n\n**not bold**\n[not a link](https://example.test)'
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown(
            `<Thoughts>${recent}</Thoughts>`,
            character,
            'normal',
            -1,
            {},
            { streamingThoughtMode: 'recent' },
        )
        expect(
            root.querySelector('.x-risu-streaming-thought-text')?.textContent,
        ).toBe(recent)
        expect(root.querySelector('strong, a')).toBeNull()
    })
    it('applies effects to the entire original before limiting the live preview', async () => {
        const original =
            '<Thoughts>' +
            'OLD '.repeat(125_000) +
            'LATEST</Thoughts>\n**Answer**'
        vi.mocked(processScriptFull).mockImplementationOnce(
            async (_char, data) => ({
                data: data.replace('LATEST', 'PROCESSED'),
                emoChanged: false,
            }),
        )
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown(
            original,
            character,
            'normal',
            -1,
            {},
            { streamingThoughtMode: 'recent' },
        )
        expect(vi.mocked(processScriptFull).mock.calls[0][1]).toBe(original)
        const preview = root.querySelector('[data-streaming-thought-preview]')
        expect(preview?.textContent).toContain('PROCESSED')
        expect(preview!.textContent!.length).toBeLessThan(1000)
        expect(root.querySelector('strong')?.textContent).toBe('Answer')
        expect(root.querySelector('details')).toBeNull()
    })
    it.each(['recent', 'collapsed', 'off'] as const)(
        'lets a display regex remove the whole thought in %s mode',
        async (mode) => {
            const original = '<Thoughts>hidden</Thoughts>**Answer**'
            vi.mocked(processScriptFull).mockImplementationOnce(
                async (_char, data) => ({
                    data: data.replace(/<Thoughts>[\s\S]*?<\/Thoughts>/g, ''),
                    emoChanged: false,
                }),
            )
            const root = document.createElement('div')
            root.innerHTML = await ParseMarkdown(
                original,
                character,
                'normal',
                -1,
                {},
                { streamingThoughtMode: mode },
            )
            expect(root.textContent?.trim()).toBe('Answer')
            expect(
                root.querySelector('details, [data-streaming-thought-preview]'),
            ).toBeNull()
        },
    )
    it.each(['<Thoughts>partial', '<Thoughts>complete</Thoughts>'])(
        'creates a closed block before and after the closing delimiter: %s',
        async (source) => {
            const root = document.createElement('div')
            root.innerHTML = await ParseMarkdown(
                source,
                character,
                'normal',
                -1,
                {},
                { streamingThoughtMode: 'collapsed' },
            )
            const details = root.querySelector(
                'details[data-risu-streaming-thought]',
            ) as HTMLDetailsElement
            expect(details).not.toBeNull()
            expect(details.open).toBe(false)
            expect(details.textContent).toContain(
                source.includes('partial') ? 'partial' : 'complete',
            )
        },
    )
    it('keeps canonical completed content and disables dedicated partial handling with mode off', async () => {
        const root = document.createElement('div')
        const original =
            '<Thoughts>' + 'FULL '.repeat(1000) + '</Thoughts>Answer'
        root.innerHTML = await ParseMarkdown(original, character)
        expect(root.querySelector('details')?.textContent).toContain(
            'FULL '.repeat(1000),
        )
        expect(
            root.querySelector(
                '[data-streaming-thought-preview], [data-risu-streaming-thought]',
            ),
        ).toBeNull()
        root.innerHTML = await ParseMarkdown(
            '<Thoughts>Partial',
            character,
            'normal',
            -1,
            {},
            { streamingThoughtMode: 'off' },
        )
        expect(root.querySelector('details')).toBeNull()
        expect(root.textContent?.trim()).toBe('Partial')
    })
    it('does not interpret HTML in the recent thought preview', async () => {
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown(
            '<Thoughts><img src=x onerror=alert(1)>& literal</Thoughts>',
            character,
            'normal',
            -1,
            {},
            { streamingThoughtMode: 'recent' },
        )
        expect(root.querySelector('img')).toBeNull()
        expect(root.textContent).toContain(
            '<img src=x onerror=alert(1)>& literal',
        )
    })
})
