<script lang="ts">
    import { language } from 'src/lang'
    import { DBState } from 'src/ts/stores.svelte'
    import { presetEditorDBState } from 'src/ts/storage/presetEditor.svelte'
    import { risuChatParser } from 'src/ts/parser/parser.svelte'
    import { tokenize } from 'src/ts/tokenizer'
    import Toggles from '../SideBars/Toggles.svelte'

    interface Props {
        value: string
        presetEditor?: boolean
    }

    let { value, presetEditor = false }: Props = $props()
    // Opened from the settings window, the preview reads the preset it edits.
    const database = $derived(presetEditor ? presetEditorDBState.db : DBState.db)
    let tokens = $state(0)
    let showToggles = $state(false)

    const parsed = $derived.by(() => {
        // Toggles change global chat variables, so the preview follows them.
        try {
            $state.snapshot(DBState.db.globalChatVariables)
        } catch {}
        return risuChatParser(value, { db: database })
    })

    $effect(() => {
        const text = parsed
        tokenize(text, database)
            .then((count) => { if (text === parsed) tokens = count })
            .catch(() => { if (text === parsed) tokens = 0 })
    })
</script>

<div class="flex min-h-0 flex-1 flex-col sm:flex-row">
    <div class="flex min-h-0 min-w-0 flex-1 flex-col">
        <div class="min-h-0 flex-1 overflow-y-auto px-4 py-3 text-base leading-relaxed whitespace-pre-wrap wrap-break-word sm:px-5 sm:py-4" data-text-editor-preview>{parsed}</div>
        <div class="flex shrink-0 items-center gap-3 border-t border-darkborderc px-4 py-2 text-sm text-textcolor2 sm:px-5">
            <button
                type="button"
                class="rounded-md px-2.5 py-1 transition-colors duration-200 hover:bg-selected hover:text-textcolor focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected"
                class:bg-selected={showToggles}
                class:text-textcolor={showToggles}
                aria-pressed={showToggles}
                onclick={() => (showToggles = !showToggles)}
            >{language.customPromptTemplateToggle}</button>
            <span>{language.tokens}: {tokens}</span>
        </div>
    </div>
    {#if showToggles}
        <div class="max-h-[45%] shrink-0 overflow-y-auto border-t border-darkborderc p-4 sm:max-h-none sm:w-80 sm:border-t-0 sm:border-l">
            <Toggles noContainer presetDatabase={database} />
        </div>
    {/if}
</div>
