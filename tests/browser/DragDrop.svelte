<script lang="ts">
    import LoreBookList from '../../src/lib/SideBars/LoreBook/LoreBookList.svelte'
    import RegexList from '../../src/lib/SideBars/Scripts/RegexList.svelte'
    import TriggerList from '../../src/lib/SideBars/Scripts/TriggerV1List.svelte'
    import PromptSettings from '../../src/lib/Setting/Pages/PromptSettings.svelte'
    import PersonaSettings from '../../src/lib/Setting/Pages/PersonaSettings.svelte'
    import { DBState } from './dragDropAdapters.svelte'
    import type { character, loreBook } from '../../src/ts/storage/database.svelte'

    let { kind }: { kind: string } = $props()
    function book(comment: string, extra: Partial<loreBook> = {}): loreBook {
        return { comment, key: comment, content: '', mode: 'normal', insertorder: 100,
            alwaysActive: false, secondkey: '', selective: false, ...extra }
    }
    // svelte-ignore state_referenced_locally
    const globalLore = kind === 'lore-pages'
        ? [book('Folder', { mode: 'folder', key: 'folder' }), book('Child', { folder: 'folder' }),
            ...Array.from({ length: 130 }, (_, i) => book(`Item ${i}`))]
        : [book('Folder', { mode: 'folder', key: 'folder' }), book('Child', { folder: 'folder' }), book('First'), book('Last')]
    DBState.db = {
        characters: [{ globalLore, chats: [{ localLore: [] }], chatPage: 0,
            triggerscript: ['First', 'Second', 'Last'].map(comment => ({ comment, type: 'start', conditions: [], effect: [] })) }],
        globalscript: ['First', 'Second', 'Last'].map(comment => ({ comment, in: '', out: '', type: 'editinput' })),
        promptTemplate: ['First', 'Second', 'Last'].map(name => ({ name, type: 'plain', text: '', role: 'system', type2: 'normal' })),
        promptSettings: {},
        personas: ['First', 'Second', 'Last'].map(name => ({ id: name, name, icon: '', personaPrompt: '', note: '' })),
        selectedPersona: 0, userIcon: '', username: 'First', userNote: '', personaPrompt: '',
    } as any
</script>

<main id="drag-fixture">
    {#if kind === 'lore' || kind === 'lore-pages'}
        <LoreBookList />
    {:else if kind === 'regex'}
        <RegexList bind:value={DBState.db.globalscript} />
    {:else if kind === 'trigger'}
        <TriggerList bind:value={(DBState.db.characters[0] as character).triggerscript} />
    {:else if kind === 'persona'}
        <PersonaSettings />
    {:else}
        <PromptSettings />
    {/if}
</main>

<style>
    /* The theme color the lorebook drop marks are drawn in. */
    :global(:root) { --risu-theme-borderc: #6272a4; }
    main { width: 500px; max-width: 95vw; margin: 8px; font-family: sans-serif; }
    :global(#drag-fixture [data-risu-idx]), :global(#drag-fixture [data-risu-idx2]) { border: 1px solid #aaa; padding: 8px; margin: 8px; }
    :global(#drag-fixture [data-show-folder]) { padding: 10px; min-height: 32px; border: 1px solid #aaa; }
    :global(#drag-fixture button) { padding: 5px; }
    :global(#drag-fixture svg) { width: 18px; height: 18px; }
    :global(#drag-fixture [role="doc-pagebreak"]) { height: 12px; }
    :global(#drag-fixture [role="doc-pagebreak"] + div) { border: 1px solid #aaa; padding: 10px; }
    :global(#drag-fixture [draggable="true"]) { min-height: 30px; }
    :global(#drag-fixture input) { display: block; }
    :global(#drag-fixture button[data-risu-idx]) { width: 60px; height: 60px; }
</style>
