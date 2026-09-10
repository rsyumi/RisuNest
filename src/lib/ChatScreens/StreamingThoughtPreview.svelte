<script lang="ts">
    import { language } from '../../lang'
    import {
        getStreamingThoughtPreview,
        type StreamingThoughtPreview,
    } from '../../ts/parser/streamingThoughtPreview'
    import type { StreamingThoughtMode } from '../../ts/storage/database.svelte'

    let {
        preview,
        mode = 'recent',
        source = '',
    }: {
        preview: StreamingThoughtPreview
        mode?: StreamingThoughtMode
        source?: string
    } = $props()
    let expanded = $state(false)
</script>

{#if preview.before}<span class="whitespace-pre-wrap">{preview.before}</span
    >{/if}
{#if mode === 'collapsed'}
    <details
        class="x-risu-streaming-thought-preview"
        bind:open={expanded}
        data-streaming-thought-preview
    >
        <summary>{language.cot}</summary>
        {#if expanded}
            <span class="whitespace-pre-wrap"
                >{getStreamingThoughtPreview(source, true)?.full ??
                    preview.recent}</span
            >
        {/if}
    </details>
{:else}
    <span
        class="x-risu-streaming-thought-preview"
        role="note"
        aria-label={language.cot}
        data-streaming-thought-preview
    >
        <span class="x-risu-streaming-thought-label">{language.cot}</span>
        <span
            class="x-risu-streaming-thought-window"
            data-truncated={preview.truncated}
        >
            <span class="x-risu-streaming-thought-text"
                >{preview.recent || '…'}</span
            >
        </span>
    </span>
{/if}
{#if preview.after}<span class="whitespace-pre-wrap">{preview.after}</span>{/if}
