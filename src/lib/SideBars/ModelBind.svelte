<script lang="ts">
    import { ChevronDownIcon } from '@lucide/svelte'
    import { DBState } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import ModelList from '../UI/ModelList.svelte'
    import CheckInput from '../UI/GUI/CheckInput.svelte'
    let expanded = $state(false)
</script>

<div class="flex flex-col gap-1 w-full">
    <div class="text-xs text-textcolor2">
        {language.model}/{language.submodel} · {language.globalSettings}
    </div>
    <ModelList compact bind:value={DBState.db.aiModel} />
    <div class="flex gap-1 items-stretch">
        <div class="min-w-0 flex-1"><ModelList compact bind:value={DBState.db.subModel} /></div>
        <button
            class="px-2 rounded-md hover:bg-selected"
            title={language.seperateModelsForAxModels}
            aria-expanded={expanded}
            onclick={() => {
                expanded = !expanded
            }}><ChevronDownIcon size={16} class={expanded ? 'rotate-180' : ''} /></button
        >
    </div>
    {#if expanded}
        <div class="flex flex-col gap-1 pl-2 border-l border-selected">
            <CheckInput
                name={language.seperateModelsForAxModels}
                bind:check={DBState.db.seperateModelsForAxModels}
            />
            {#each [['memory', language.axModelMemory], ['translate', language.axModelTranslate], ['emotion', language.axModelEmotion], ['otherAx', language.axModelOther]] as [key, label]}
                <span class="text-xs text-textcolor2">{label}</span>
                <ModelList
                    compact
                    blankable
                    disabled={!DBState.db.seperateModelsForAxModels}
                    value={DBState.db.seperateModels?.[key] ?? ''}
                    onChange={(value) => {
                        DBState.db.seperateModels ??= { memory: '', translate: '', emotion: '', otherAx: '' }
                        DBState.db.seperateModels[key] = value
                    }}
                />
            {/each}
        </div>
    {/if}
</div>
