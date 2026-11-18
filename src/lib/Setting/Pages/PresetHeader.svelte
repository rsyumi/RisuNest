<script lang="ts">
    import ListIcon from '@lucide/svelte/icons/list'
    import { language } from 'src/lang'
    import { DBState, openPresetList } from 'src/ts/stores.svelte'
    import Button from 'src/lib/UI/GUI/Button.svelte'

    let preset = $derived(DBState.db.botPresets?.[DBState.db.botPresetsId])
</script>

<div class="flex items-center gap-3 rounded-md border border-darkborderc bg-darkbg p-2 mb-4">
    {#if preset?.image}
        <img src={preset.image} alt="" class="size-10 shrink-0 rounded-md object-cover" decoding="async" draggable="false" />
    {/if}
    <div class="min-w-0 flex-1">
        <div class="text-xs text-subtext">{language.presets}</div>
        <div class="font-medium truncate">{preset?.name ?? ''}</div>
    </div>
    <Button size="md" className="text-sm inline-flex items-center gap-1.5 shrink-0" onclick={() => { $openPresetList = true }}>
        <ListIcon size={16} />{language.risuNest.presetList.change}
    </Button>
</div>
