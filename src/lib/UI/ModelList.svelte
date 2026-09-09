<script lang="ts">
    
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import { DBState } from 'src/ts/stores.svelte';
    import { getHordeModels } from "src/ts/horde/getModels";
    import Accordion from "./Accordion.svelte";
    import { language } from "src/lang";
    import CheckInput from "./GUI/CheckInput.svelte";
    import { getModelInfo, getModelList } from 'src/ts/model/modellist';
    import { ArrowLeft } from "@lucide/svelte";

    interface Props {
        value?: string
        onChange?: (v: string) => void
        onclick?: (
            event: MouseEvent & {
                currentTarget: EventTarget & HTMLDivElement
            },
        ) => any
        blankable?: boolean
        excludesPrefix?: string
        noMargin?: boolean
        compact?: boolean
        disabled?: boolean
    }

    let {
        value = $bindable(''),
        onChange = (v) => {},
        onclick,
        blankable,
        excludesPrefix,
        noMargin,
        compact = false,
        disabled = false,
    }: Props = $props()
    let openOptions = $state(false)
    let activeTab = $state<'base' | 'plugin'>('base')
    const allowed = (id: string) => !excludesPrefix || !id.startsWith(excludesPrefix)

    function changeModel(name: string) {
        value = name
        openOptions = false
        onChange(name)
    }
    let showUnrec = $state(false)
    let providers = $derived(getModelList({
        recommendedOnly: !showUnrec,
        groupedByProvider: true
    }))
    let pluginModels = $derived(
        providers.find((group) => group.providerName === 'Plugins')?.models.filter((m) => allowed(m.id)) ??
            [],
    )
    let baseProviders = $derived(providers.filter((group) => group.providerName !== 'Plugins'))
</script>

{#if openOptions}
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <div use:modalNavigation={{ close: () => { openOptions = false } }} class="fixed top-0 w-full h-full left-0 bg-black/50 z-modal flex justify-center items-center" role="button" tabindex="0" onclick={() => {
        openOptions = false
    }}>
        <div class="w-96 max-w-full max-h-full overflow-y-auto overflow-x-hidden bg-bgcolor p-4 flex flex-col" role="button" tabindex="0" onclick={(e)=>{
            e.stopPropagation()
            onclick?.(e)
        }}>
            <div class="flex items-center gap-3 mb-4">
                <button 
                    class="flex items-center justify-center p-2 rounded-lg hover:bg-selected transition-colors shrink-0"
                    onclick={() => {
                        openOptions = false
                    }}
                    title="Back"
                >
                    <ArrowLeft size={20} />
                </button>
                <h1 class="font-bold text-xl flex-1">{language.model}</h1>
            </div>
            <div class="border-t-1 border-y-selected mb-2"></div>

            <div class="flex gap-1 mb-2" role="tablist" aria-label={language.model}>
                <button role="tab" aria-selected={activeTab === 'base'} class="flex-1 rounded-md p-2 hover:bg-selected" class:bg-selected={activeTab === 'base'} onclick={() => {activeTab = 'base'}}>{language.model}</button>
                <button role="tab" aria-selected={activeTab === 'plugin'} class="flex-1 rounded-md p-2 hover:bg-selected" class:bg-selected={activeTab === 'plugin'} onclick={() => {activeTab = 'plugin'}}>{language.plugin}</button>
            </div>
            {#if activeTab === 'plugin'}
                {#if value.startsWith('pluginmodel:::') && !pluginModels.some(model => model.id === value)}
                    <p class="text-textcolor2 text-sm">{language.pluginModelUnavailable}</p>
                {/if}
                {#each pluginModels as model}
                    <button class="hover:bg-selected px-4 py-2 text-left rounded-md" class:bg-selected={model.id === value} onclick={() => changeModel(model.id)}>{model.name}</button>
                {:else}
                    <p class="text-textcolor2 text-sm p-2">{language.noPluginModels}</p>
                {/each}
            {:else}
            {#each baseProviders as provider}
                {#if provider.providerName === '@as-is'}
                    {#each provider.models.filter(m => allowed(m.id)) as model}
                        <button class="hover:bg-selected px-6 py-2 text-lg" onclick={() => {changeModel(model.id)}}>{model.name}</button>
                    {/each}
                {:else}
                    <Accordion name={provider.providerName}>
                        {#each provider.models.filter(m => !excludesPrefix || !m.id.startsWith(excludesPrefix)) as model}
                            <button class="hover:bg-selected px-6 py-2 text-lg" onclick={() => {changeModel(model.id)}}>{model.name}</button>
                        {/each}
                    </Accordion>
                {/if}
            {/each}
            {#if allowed('horde:::')}
            <Accordion name="Horde">
                {#await getHordeModels()}
                    <button class="p-2">Loading...</button>
                {:then models}
                    <button onclick={() => {changeModel("horde:::" + 'auto')}} class="p-2 hover:text-green-500">
                        Auto Model
                        <br><span class="text-textcolor2 text-sm">Performace: Auto</span>
                    </button>
                    {#each models as model}
                        <button onclick={() => {changeModel("horde:::" + model.name)}} class="p-2 hover:text-green-500">
                            {model.name.trim()}
                            <br><span class="text-textcolor2 text-sm">Performace: {model.performance.toFixed(1)}</span>
                        </button>
                    {/each}
                {/await}
            </Accordion>

            {/if}
            {#if DBState?.db.customModels?.length > 0}
                <Accordion name={language.customModels}>
                    {#each DBState.db.customModels.filter(m => allowed(m.id)) as model}
                        <button class="hover:bg-selected px-6 py-2 text-lg" onclick={() => {changeModel(model.id)}}>{model.name ?? "Unnamed"}</button>
                    {/each}
                </Accordion>

            {/if}

            

            {/if}
            {#if blankable}
                <button class="hover:bg-selected px-6 py-2 text-lg" onclick={() => {changeModel('')}}>{language.none}</button>
            {/if}
            {#if activeTab === 'base'}
            <div class="text-textcolor2 text-xs">
                <CheckInput name={language.showUnrecommended}  grayText bind:check={showUnrec}/>
            </div>
            {/if}
        </div>
    </div>

{/if}

<button {disabled} onclick={() => {activeTab = value.startsWith('pluginmodel:::') ? 'plugin' : 'base'; openOptions = true}}
    class={{
        "drop-shadow-lg p-3 flex justify-center items-center ml-2 mr-2 rounded-lg bg-darkbutton border-darkborderc border": !compact,
        "w-full min-h-10 px-3 py-2 text-sm rounded-md border border-darkborderc bg-darkbutton truncate text-left": compact,
        "my-4": !noMargin && !compact,
    }}>
        {getModelInfo(value)?.fullName || language.none}
</button>

