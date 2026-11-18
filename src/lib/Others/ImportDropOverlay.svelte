<script lang="ts">
    import FileDownIcon from '@lucide/svelte/icons/file-down'
    import { language } from 'src/lang'
    import { importDropOverlayVisible } from 'src/ts/importIntake/sheetStore'
</script>

<!-- The overlay never takes pointer events, so the drag keeps reaching the document's drop handlers. -->
{#if $importDropOverlayVisible}
    <div class="pointer-events-none fixed inset-0 z-drop-overlay flex items-center justify-center bg-black/60 p-6" data-testid="import-drop-overlay">
        <div class="flex h-full w-full flex-col items-center justify-center gap-3 rounded-xl border-2 border-dashed border-borderc text-textcolor">
            <FileDownIcon size={40} class="text-textcolor" aria-hidden="true" />
            <span class="text-lg font-bold">{language.risuNest.importIntake.dropHint}</span>
        </div>
    </div>
{/if}
