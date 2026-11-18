<script lang="ts">
    import type { sidebarToggle, sidebarToggleGroup } from "src/ts/util";
    import Accordion from '../UI/Accordion.svelte'
    import SwitchInput from "../UI/GUI/SwitchInput.svelte";
    import SelectInput from "../UI/GUI/SelectInput.svelte";
    import OptionInput from "../UI/GUI/OptionInput.svelte";
    import TextAreaInput from '../UI/GUI/TextAreaInput.svelte'
    import TextInput from "../UI/GUI/TextInput.svelte";

    interface Props {
        /** Grouped toggle definitions, as `groupSidebarToggles` returns them. */
        items: sidebarToggle[]
        reverse?: boolean
        /** The value of a `toggle_` variable, '' when it has none. */
        value: (name: string) => string
        onEdit: (name: string, value: string) => void
        /** Whether a `toggle_` variable cannot be edited now. */
        blocked?: (name: string) => boolean
        /** Whether the toggle with this definition key differs from the chat's saved values. */
        dirty?: (key: string | undefined) => boolean
        /** Whether a `toggle_` variable has a value set only in this chat. */
        pinned?: (name: string) => boolean
        pinDisabled?: boolean
        onUnpin?: (name: string) => void
    }

    let {
        items,
        reverse = false,
        value,
        onEdit,
        blocked = () => false,
        dirty = () => false,
        pinned = () => false,
        pinDisabled = false,
        onUnpin = () => {},
    }: Props = $props();

    const dirtyClass = (key: string | undefined) => (dirty(key) ? 'bg-draculared/15' : '')

    // Switch rows sit in the same list as the select/text rows below, which set their own spacing.
    const switchRow = 'mt-2 px-1'
</script>

{#snippet localToggle(toggle: sidebarToggle)}
    {#if pinned(`toggle_${toggle.key}`)}
        <button
            disabled={pinDisabled}
            onclick={() => {
                onUnpin(`toggle_${toggle.key}`)
            }}
        >
            📌
        </button>
    {/if}
{/snippet}

{#snippet getToggleDisplayName(toggle: sidebarToggle)}
    {toggle.value}{@render localToggle(toggle)}
{/snippet}

{#snippet toggles(definitions: sidebarToggle[], reverse: boolean = false)}
    <!-- A group without toggles has nothing to show; drawn as a switch it would write `toggle_undefined`. -->
    {@const items = definitions.filter((toggle) => toggle.type !== 'group' || toggle.children.length > 0)}
    {#each items as toggle, index}
        {#if index > 0 && toggle.type !== 'divider' && items[index - 1]?.type !== 'divider' && toggle.type !== 'caption' && items[index - 1]?.type !== 'caption' && !(toggle.type === 'group' && items[index - 1]?.type === 'group')}
            <div class="w-full my-2 border-t border-darkborderc/20"></div>
        {/if}
        {#if toggle.type === 'group' && toggle.children.length > 0}
            <div class="w-full mt-1">
                <Accordion styled name={toggle.value}>
                    {@render toggles((toggle as sidebarToggleGroup).children, reverse)}
                </Accordion>
            </div>
        {:else if toggle.type === 'select'}
            <div inert={blocked(`toggle_${toggle.key}`)} class="w-full flex gap-2 mt-2 items-center justify-between min-h-10 rounded-md px-1 transition-colors {dirtyClass(toggle.key)}">
                <span class="min-w-0 break-words">{@render getToggleDisplayName(toggle)}</span>
                <SelectInput
                    className="w-32 shrink-0"
                    value={value(`toggle_${toggle.key}`)}
                    onchange={(e) => {
                        onEdit(`toggle_${toggle.key}`, e.currentTarget.value)
                    }}
                >
                    {#each toggle.options as option, i}
                        <OptionInput value={i.toString()}>{option}</OptionInput>
                    {/each}
                </SelectInput>
            </div>
        {:else if toggle.type === 'text'}
            <div class="w-full flex gap-2 mt-2 items-center justify-between min-h-10 rounded-md px-1 transition-colors {dirtyClass(toggle.key)}">
                <span class="min-w-0 break-words">{@render getToggleDisplayName(toggle)}</span>
                <TextInput
                    className="w-32 shrink-0"
                    disabled={blocked(`toggle_${toggle.key}`)}
                    value={value(`toggle_${toggle.key}`)}
                    onchange={(e) => {
                        onEdit(`toggle_${toggle.key}`, e.currentTarget.value)
                    }}
                />
            </div>
        {:else if toggle.type === 'textarea'}
            <div inert={blocked(`toggle_${toggle.key}`)} class="w-full flex gap-2 mt-2 items-start justify-between min-h-10 rounded-md px-1 transition-colors {dirtyClass(toggle.key)}">
                <span class="min-w-0 break-words mt-1.5">{@render getToggleDisplayName(toggle)}</span>
                <TextAreaInput
                    className="w-32 shrink-0"
                    height="20"
                    value={value(`toggle_${toggle.key}`)}
                    onchange={(e) => {
                        //check is div
                        if (e.currentTarget instanceof HTMLDivElement) {
                            onEdit(`toggle_${toggle.key}`, e.currentTarget.innerText)
                        } else {
                            onEdit(`toggle_${toggle.key}`, e.currentTarget.value)
                        }
                    }}
                />
            </div>
        {:else if toggle.type === 'caption'}
            <div class="w-full mt-1 text-xs text-textcolor2">
                {toggle.value}
            </div>
        {:else if toggle.type === 'divider'}
            <!-- Prevent multiple dividers appearing in a row -->
            {#if index === 0 || items[index - 1]?.type !== 'divider' || items[index - 1]?.value !== toggle.value}
                <div class="w-full min-h-5 flex gap-2 mt-2 items-center" class:justify-end={!reverse}>
                    {#if toggle.value}
                        <span class="shrink-0">{@render getToggleDisplayName(toggle)}</span>
                    {/if}
                    <hr class="border-t border-darkborderc m-0 grow" />
                </div>
            {/if}
        {:else}
            <SwitchInput
                className={switchRow}
                disabled={blocked(`toggle_${toggle.key}`)}
                check={value(`toggle_${toggle.key}`) === '1'}
                name={toggle.value}
                highlight={dirty(toggle.key)}
                onChange={(checked) => {
                    onEdit(`toggle_${toggle.key}`, checked ? '1' : '0')
                }}
            >
                {@render localToggle(toggle)}
            </SwitchInput>
        {/if}
    {/each}
{/snippet}

{@render toggles(items, reverse)}
