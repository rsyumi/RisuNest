<script lang="ts">
    import { onMount, tick } from 'svelte'
    import Chats from '../../src/lib/ChatScreens/Chats.svelte'
    import { DBState, selectedCharID } from '../../src/ts/stores.svelte'
    import type { character } from '../../src/ts/storage/database.svelte'
    import {
        FIXTURE_CHARACTER_COUNT,
        FIXTURE_MESSAGE_COUNT,
        createFixtureCharacters,
    } from './fixture'

    interface RenderVerification {
        validOutput: boolean
        mountedMessageCount: number
        firstMountedIndex: number
        lastMountedIndex: number
        styledOutput: boolean
        outputSignature: string
    }

    interface Props {
        onFirstChatReady: (verification: RenderVerification) => void
    }

    let { onFirstChatReady }: Props = $props()
    const characters = createFixtureCharacters()
    let activeIndex = $state(0)
    let currentCharacter = $derived(characters[activeIndex])
    const EXPECTED_MOUNTED_MESSAGE_COUNT = 64

    DBState.db = {
        characters,
        username: 'Synthetic User',
        userIcon: '',
        modules: [],
        enabledModules: [],
        globalscript: [],
        translator: '',
        translatorType: 'google',
        autoTranslate: false,
        swipe: false,
        showFirstMessagePages: false,
        enableBookmark: false,
        useChatCopy: false,
        clickToEdit: false,
        enableBlockPartialEdit: false,
        enableDragPartialEdit: false,
        requestInfoInsideChat: false,
        showMemoryLimit: false,
        memoryLimitThickness: 0,
        createFolderOnBranch: false,
        alwaysScrollToNewMessage: false,
        autoScrollToNewMessage: false,
        theme: '',
        iconsize: 100,
        zoomsize: 100,
        lineHeight: 1.25,
        guiHTML: '',
        roundIcons: false,
        hideAllImages: true,
        returnCSSError: false,
        customQuotes: false,
        customQuotesData: ['', '', '', ''],
        unformatQuotes: false,
        blockquoteStyling: false,
        assetWidth: -1,
        legacyMediaFindings: false,
        assetMaxDifference: 0,
        dynamicAssets: false,
        dynamicAssetsEditDisplay: false,
        aiLawApplies: false,
        moduleIntergration: '',
        selectedPersona: 0,
        personas: [
            {
                name: 'Synthetic User',
                personaPrompt: '',
                icon: '',
                note: '',
                largePortrait: false,
            },
        ],
    } as typeof DBState.db
    selectedCharID.set(0)

    function fnv1a(value: string): string {
        let hash = 0x811c9dc5
        for (let index = 0; index < value.length; index += 1) {
            hash ^= value.charCodeAt(index)
            hash = Math.imul(hash, 0x01000193)
        }
        return (hash >>> 0).toString(16).padStart(8, '0')
    }

    function verifyRenderedCharacter(
        characterIndex: number,
    ): RenderVerification {
        const rows = [
            ...document.querySelectorAll<HTMLElement>(
                '[data-chat-render-key][data-chat-index]',
            ),
        ]
            .map((element) => ({
                element,
                index: Number(element.dataset.chatIndex),
            }))
            .sort((left, right) => left.index - right.index)
        const firstMountedIndex =
            FIXTURE_MESSAGE_COUNT - EXPECTED_MOUNTED_MESSAGE_COUNT
        const allRowsComplete = rows.every(({ element, index }, offset) => {
            const normalizedText = (element.textContent ?? '')
                .replace(/\s+/g, ' ')
                .trim()
            return (
                index === firstMountedIndex + offset &&
                normalizedText.includes(
                    `UIBENCH_RENDERED_${characterIndex}_${index}`,
                ) &&
                Boolean(element.querySelector('pre code')) &&
                Boolean(element.querySelector('table'))
            )
        })
        const signatureInput = rows
            .map(({ element, index }) => {
                const normalizedText = (element.textContent ?? '')
                    .replace(/\s+/g, ' ')
                    .trim()
                return `${index}:${normalizedText}`
            })
            .join('|')
        const firstChatRoot = rows[0]?.element.firstElementChild
        const styledOutput =
            getComputedStyle(document.body).backgroundColor ===
                'rgb(40, 42, 54)' &&
            firstChatRoot instanceof HTMLElement &&
            getComputedStyle(firstChatRoot).display === 'flex'
        return {
            validOutput:
                rows.length === EXPECTED_MOUNTED_MESSAGE_COUNT &&
                allRowsComplete &&
                rows.at(0)?.index === firstMountedIndex &&
                rows.at(-1)?.index === FIXTURE_MESSAGE_COUNT - 1 &&
                styledOutput,
            mountedMessageCount: rows.length,
            firstMountedIndex: rows.at(0)?.index ?? -1,
            lastMountedIndex: rows.at(-1)?.index ?? -1,
            styledOutput,
            outputSignature: fnv1a(`${signatureInput}|styled:${styledOutput}`),
        }
    }

    async function waitForRenderedCharacter(
        characterIndex: number,
    ): Promise<RenderVerification> {
        const deadline = performance.now() + 20_000
        while (performance.now() < deadline) {
            await tick()
            const verification = verifyRenderedCharacter(characterIndex)
            if (verification.validOutput) return verification
            await new Promise<void>((resolve) =>
                requestAnimationFrame(() => resolve()),
            )
        }
        throw new Error(
            'Synthetic chat did not render before the bounded deadline',
        )
    }

    export async function navigateToNextCharacter(): Promise<RenderVerification> {
        activeIndex = (activeIndex + 1) % FIXTURE_CHARACTER_COUNT
        selectedCharID.set(activeIndex)
        return waitForRenderedCharacter(activeIndex)
    }

    onMount(() => {
        void waitForRenderedCharacter(0).then(onFirstChatReady)
    })
</script>

<main id="ui-benchmark-root">
    <div class="benchmark-scroll-shell">
        <Chats
            messages={currentCharacter.chats[0].message}
            {currentCharacter}
            onReroll={() => {}}
            unReroll={() => {}}
            currentUsername="Synthetic User"
            userIcon=""
        />
    </div>
</main>

<style>
    :global(html),
    :global(body),
    :global(#app),
    #ui-benchmark-root {
        height: 100%;
        margin: 0;
    }

    :global(body) {
        font-family: system-ui, sans-serif;
        overflow: hidden;
    }

    .benchmark-scroll-shell {
        box-sizing: border-box;
        height: 100%;
        overflow-y: auto;
        padding: 12px;
    }
</style>
