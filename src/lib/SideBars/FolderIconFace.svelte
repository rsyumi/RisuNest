<script lang="ts">
    import FolderIcon from '@lucide/svelte/icons/folder'
    import FolderOpenIcon from '@lucide/svelte/icons/folder-open'
    import { canvasLineMeasure, fitFolderIconText, folderIconLineHeight } from 'src/ts/gui/folderIconFit'
    import { folderTextStyle, type FolderAppearance } from 'src/ts/gui/folderAppearance'

    interface Props {
        appearance: FolderAppearance
        round: boolean
        open?: boolean
    }

    let { appearance, round, open = false }: Props = $props()

    let element: HTMLDivElement | undefined = $state()
    let font = $state('')
    let fontEpoch = $state(0)

    $effect(() => {
        if (!element) return
        font = getComputedStyle(element).fontFamily
        let live = true
        // A web font that finishes loading changes every width.
        document.fonts?.ready.then(() => {
            if (!live || !element) return
            font = getComputedStyle(element).fontFamily
            fontEpoch++
        })
        return () => { live = false }
    })

    const text = $derived(folderTextStyle(appearance))
    const fit = $derived.by(() => {
        void fontEpoch
        return fitFolderIconText(appearance.text, round, canvasLineMeasure(font))
    })
</script>

<div
    bind:this={element}
    class="folder-icon-face"
    class:shadowed={text.shadow !== null}
    style:color={text.color}
    style:--folder-icon-shadow={text.shadow}
>
    {#if appearance.iconType === 'glyph'}
        {#if open}
            <FolderOpenIcon />
        {:else}
            <FolderIcon />
        {/if}
    {:else if appearance.iconType === 'text'}
        <div class="lines" style:font-size="{fit.size}px" style:line-height={folderIconLineHeight}>
            {#each fit.lines as line, index (index)}
                <div class:clip={fit.clipped} style:max-width={fit.clipped ? `${fit.maxWidths[index]}px` : null}>{line}</div>
            {/each}
        </div>
    {/if}
</div>

<style>
    .folder-icon-face {
        display: flex;
        height: 100%;
        width: 100%;
        align-items: center;
        justify-content: center;
        overflow: hidden;
        font-weight: 700;
        text-align: center;
    }
    .shadowed .lines {
        text-shadow: 0 0 2px var(--folder-icon-shadow), 0 0 1px var(--folder-icon-shadow);
    }
    .shadowed :global(svg) {
        filter: drop-shadow(0 0 1px var(--folder-icon-shadow));
    }
    .lines {
        display: flex;
        flex-direction: column;
        align-items: center;
        white-space: nowrap;
    }
    .clip {
        overflow: hidden;
        text-overflow: ellipsis;
    }
</style>
