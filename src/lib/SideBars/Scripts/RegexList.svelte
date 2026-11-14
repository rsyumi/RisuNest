<script lang="ts">
    import type { customscript } from "src/ts/storage/database.svelte";
    import RegexData from "./RegexData.svelte";
    import Sortable from "sortablejs";
    import { sortableOptions } from "src/ts/util";
    import { abandonSortDrag, destroySortableOnClose, watchSortDrag } from "src/ts/sortableOptions";
    import { onDestroy, onMount } from "svelte";
  import { DownloadIcon, HardDriveUploadIcon, PlusIcon } from "@lucide/svelte";
  import { exportRegex, importRegex } from "src/ts/process/scripts";
    interface Props {
        value?: customscript[];
        buttons?: boolean
        onImport?: () => Promise<void>
    }

    let { value = $bindable([]), buttons = false, onImport }: Props = $props();
    let stb: Sortable = null
    let ele: HTMLDivElement = $state()
    let originalNextSibling: Node | null = null
    let dragWatch: ReturnType<typeof watchSortDrag> | null = null
    let opened = $state(new Set<customscript>())
    const createStb = () => {
        stb = Sortable.create(ele, {
            draggable: '> [data-risu-idx]',
            onStart: (event) => {
                originalNextSibling = event.item.nextSibling
                dragWatch = watchSortDrag()
            },
            onEnd: (event) => {
                const cancelled = dragWatch?.cancelled(event)
                dragWatch?.stop()
                dragWatch = null
                const newValue = Array.from(ele.children)
                    .filter(row => row.hasAttribute('data-risu-idx'))
                    .map(row => value[Number(row.getAttribute('data-risu-idx'))])
                // Restore Svelte's DOM before applying the new order.
                event.from.insertBefore(event.item, originalNextSibling)
                // An abandoned drag changes nothing.
                if (cancelled) return
                value = newValue
            },
            ...sortableOptions
        })
    }

    const onOpen = (item: customscript) => {
        if (opened.has(item)) return
        opened = new Set([...opened, item])
        if (stb) {
            abandonSortDrag()
            stb.destroy()
            stb = null
        }
    }
    const onClose = (item: customscript) => {
        if (!opened.has(item)) return
        opened.delete(item)
        opened = new Set(opened)
        if (opened.size === 0) createStb()
    }

    $effect(() => {
        for (const item of opened) {
            if (!value.includes(item)) onClose(item)
        }
    })

    onMount(createStb)

    onDestroy(() => {
        if(stb){
            try {
                destroySortableOnClose(stb)
            } catch (error) {}
        }
    })
</script>
    <div class="contain w-full max-w-full mt-2 flex flex-col p-3 border-selected border-1 bg-darkbg rounded-md" bind:this={ele}>
        {#if value.length === 0}
                <div class="text-textcolor2">No Scripts</div>
        {/if}
        {#each value as customscript, i (customscript)}
            <RegexData idx={i} bind:value={value[i]} onOpen={() => onOpen(customscript)} onClose={() => onClose(customscript)} onRemove={() => {
                let customscript = value
                customscript.splice(i, 1)
                value = customscript
            }}/>
        {/each}
    </div>
{#if buttons}
    <div class="flex gap-2 mt-2">
        <button class="rounded-md text-textcolor2 hover:text-textcolor focus-within:text-textcolor" onclick={() => {
            value.push({
            comment: "",
            in: "",
            out: "",
            type: "editinput"
            })
        }}>
            <PlusIcon />
        </button>
        <button class="rounded-md text-textcolor2 hover:text-textcolor focus-within:text-textcolor" onclick={() => {
            exportRegex(value)
        }}><DownloadIcon /></button>
        <button class="rounded-md text-textcolor2 hover:text-textcolor focus-within:text-textcolor" onclick={async () => {
            if (onImport) await onImport()
            else { const scripts = await importRegex(); if (scripts.length) value = [...value, ...scripts] }
        }}><HardDriveUploadIcon /></button>
    </div>
{/if}