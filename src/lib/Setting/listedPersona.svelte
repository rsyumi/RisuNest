<script lang="ts">
    import { XIcon } from "@lucide/svelte";
    import { language } from "../../lang";
    
    import { DBState } from 'src/ts/stores.svelte';
    import { changeUserPersona } from "src/ts/persona";


    interface Props {
        close?: () => void
        bindingMode?: boolean
        selectedId?: string
        onSelect?: (index: number) => void | Promise<void>
    }

    let { close = () => {}, bindingMode = false, selectedId, onSelect }: Props = $props()

</script>

<div class="absolute w-full h-full z-40 bg-black/50 flex justify-center items-center">
    <div class="bg-darkbg p-4 break-any rounded-md flex flex-col max-w-3xl w-96 max-h-full overflow-y-auto">
        <div class="flex items-center text-textcolor mb-4">
            <h2 class="mt-0 mb-0 font-bold">{language.persona}</h2>
            <div class="grow flex justify-end">
                <button title={language.cancel} class="text-textcolor2 hover:text-green-500 mr-2 cursor-pointer items-center" onclick={close}>
                    <XIcon size={24}/>
                </button>
            </div>
        </div>
        {#if bindingMode}
            <button class="p-2 text-left hover:bg-selected" class:bg-selected={!selectedId} onclick={() => {onSelect?.(-1); close()}}>{language.inheritPersona} ({DBState.db.username || DBState.db.personas[DBState.db.selectedPersona]?.name || ''})</button>
        {/if}
        {#each DBState.db.personas as persona, i}
            <button onclick={() => {
                if (bindingMode) onSelect?.(i)
                else changeUserPersona(i)
                close()
            }} class="flex items-center text-textcolor border-t-1 border-solid border-0 border-darkborderc p-2 cursor-pointer" class:bg-selected={bindingMode ? !!persona.id && persona.id === selectedId : i === DBState.db.selectedPersona}>
                <span class="overflow-x-auto whitespace-nowrap w-full text-left">
                    <span class="font-medium">{persona.name}</span>
                    {#if persona.note}
                        <span class="opacity-75"> / {persona.note}</span>
                    {/if}
                </span>
            </button>
        {/each}
    </div>
</div>

<style>
    .break-any{
        word-break: normal;
        overflow-wrap: anywhere;
    }
</style>