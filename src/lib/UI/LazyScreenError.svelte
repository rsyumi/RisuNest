<script lang="ts">
    import { language } from 'src/lang'
    let { message, onRetry, backLabel, onBack, overlay = false }: { message: string; onRetry(): void; backLabel: string; onBack(): void; overlay?: boolean } = $props()
</script>
<div class="{overlay ? 'absolute inset-0 z-40 bg-darkbg' : ''} w-full h-full flex flex-col gap-3 items-center justify-center text-textcolor" role="alert">
    <span>{message}</span>
    <p class="px-4 text-sm text-center text-subtext">{language.risuNest.lazy.restartHelp}</p>
    <div class="flex gap-2">
        <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected" onclick={onRetry}>{language.retry}</button>
        <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected" onclick={onBack}>{backLabel}</button>
    </div>
</div>
