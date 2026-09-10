<script lang="ts">
    import { untrack } from 'svelte'
    import type { SettingContext, SettingItem } from 'src/ts/setting/types'
    import { UNINITIALIZED, getLabel, getSettingValue, resolveLanguagePath, setSettingValue } from 'src/ts/setting/utils'
    import { risuNestSettingUnits } from 'src/ts/setting/risuNestSettingsData'
    import SelectInput from 'src/lib/UI/GUI/SelectInput.svelte'
    import OptionInput from 'src/lib/UI/GUI/OptionInput.svelte'
    import SliderInput from 'src/lib/UI/GUI/SliderInput.svelte'
    import NumberInput from 'src/lib/UI/GUI/NumberInput.svelte'
    import SegmentedButtons from './SegmentedButtons.svelte'
    import SettingToggle from './SettingToggle.svelte'

    interface Props {
        item: SettingItem
        ctx: SettingContext
    }

    let { item, ctx }: Props = $props()

    // Mirrors the shared wrappers: read the live value, write back only genuine changes.
    let localValue: any = $state(untrack(() => getSettingValue(item, ctx)))

    $effect(() => {
        localValue = getSettingValue(item, ctx)
    })

    $effect(() => {
        const value = localValue
        if (value === UNINITIALIZED) return
        untrack(() => {
            if (value !== getSettingValue(item, ctx)) setSettingValue(item, value, ctx)
        })
    })

    function optionLabel(option: { label?: string; labelKey?: string }): string {
        const translated = option.labelKey ? resolveLanguagePath(option.labelKey) : undefined
        return typeof translated === 'string' ? translated : (option.label ?? '')
    }

    let segmentOptions = $derived((item.options?.segmentOptions ?? [])
        .filter((option) => !option.condition || option.condition(ctx))
        .map((option) => ({ value: option.value, label: optionLabel(option) })))
    let selectOptions = $derived((item.options?.selectOptions ?? [])
        .filter((option) => !option.condition || option.condition(ctx)))
    let unit = $derived(risuNestSettingUnits[item.id])
</script>

{#if item.type === 'segmented'}
    <SegmentedButtons bind:value={localValue} options={segmentOptions} label={getLabel(item)} />
{:else if item.type === 'check'}
    <SettingToggle bind:checked={localValue} label={getLabel(item)} />
{:else if item.type === 'select'}
    <SelectInput bind:value={localValue} ariaLabel={getLabel(item)} className="w-full @xl:w-auto @xl:min-w-44">
        {#each selectOptions as option (option.value)}
            <OptionInput value={option.value}>{optionLabel(option)}</OptionInput>
        {/each}
    </SelectInput>
{:else if item.type === 'slider'}
    <div class="w-full @xl:w-64">
        <SliderInput
            min={item.options?.min}
            max={item.options?.max}
            step={item.options?.step}
            fixed={item.options?.fixed}
            multiple={item.options?.multiple}
            disableable={item.options?.disableable}
            ariaLabel={getLabel(item)}
            bind:value={localValue}
        />
    </div>
{:else if item.type === 'number'}
    <NumberInput size="sm" min={item.options?.min} max={item.options?.max} ariaLabel={getLabel(item)} className="w-28 text-right" bind:value={localValue} />
    {#if unit}<span class="text-sm text-textcolor2">{unit}</span>{/if}
{/if}
