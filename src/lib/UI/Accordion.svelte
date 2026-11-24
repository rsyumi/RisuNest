<script lang="ts" module>
    // Open state of accordions with an `openKey`, kept in memory for the app session.
    const rememberedOpen = new Map<string, boolean>()
</script>
<script lang="ts">
    import { untrack } from "svelte";
    import type { language } from "src/lang";
    import Help from "../Others/Help.svelte";

    let open = $state(false)
    interface Props {
        name?: string;
        styled?: boolean;
        help?: (keyof (typeof language.help))|'';
        disabled?: boolean;
        children?: import('svelte').Snippet;
        className?: string;
        /** Text at the right of a styled header. */
        summary?: string;
        /** Remembers the open state under this key for the app session. */
        openKey?: string;
        startOpen?: boolean;
    }

    let {
        name = "",
        styled = false,
        help = '',
        disabled = false,
        children,
        className = "",
        summary = '',
        openKey,
        startOpen = false
    }: Props = $props();

    open = untrack(() => (openKey === undefined ? undefined : rememberedOpen.get(openKey)) ?? startOpen)
    $effect(() => {
        if (openKey !== undefined) rememberedOpen.set(openKey, open)
    })
</script>
{#if disabled}
    {@render children?.()}
{:else if styled}
    <div class="flex flex-col mt-2">
        <button class="hover:bg-selected px-6 py-2 text-lg rounded-t-md border-selected border"
            class:bg-selected={open}
            class:rounded-b-md={!open}
            class:relative={!!summary}
            onclick={() => {
                open = !open
            }}
        >
            <span class="mr-2">{name}</span>
        {#if help}
            <Help key={help} />
        {/if}{#if summary}<span class="absolute right-4 inset-y-0 flex items-center text-sm text-textcolor2">{summary}</span>{/if}</button>
        {#if open}
            <div class={"flex flex-col border border-selected p-2 rounded-b-md " + className}>
                {@render children?.()}
            </div>
        {/if}
    </div>
{:else}
    <div class="flex flex-col">
        <button class="hover:bg-selected px-6 py-2 text-lg" class:bg-selected={open} onclick={() => {
            open = !open
        }}>{name}</button>
        {#if open}
            <div class="flex flex-col bg-darkbg">
                {@render children?.()}
            </div>
        {/if}
    </div>
{/if}