<script lang="ts">
  import { PlusIcon } from '@lucide/svelte'
  import Sortable from 'sortablejs'
  import type { triggerscript } from 'src/ts/storage/database.svelte'
  import { sortableOptions } from 'src/ts/util'
  import { abandonSortDrag, destroySortableOnClose, watchSortDrag } from 'src/ts/sortableOptions'
  import { onDestroy, onMount } from 'svelte'
  import TriggerData from './TriggerV1Data.svelte'

  interface Props {
    value?: triggerscript[]
    lowLevelAble?: boolean
  }

  let { value = $bindable([]), lowLevelAble = false }: Props = $props()
  let stb: Sortable = null
  let ele: HTMLDivElement = $state()
  let originalNextSibling: Node | null = null
  let dragWatch: ReturnType<typeof watchSortDrag> | null = null
  let opened = $state(new Set<triggerscript>())

  const createStb = () => {
    if (!ele) {
      return
    }
    stb = Sortable.create(ele, {
      draggable: '> [data-risu-idx2]',
      onStart: (event) => {
          originalNextSibling = event.item.nextSibling
          dragWatch = watchSortDrag()
      },
      onEnd: (event) => {
          const cancelled = dragWatch?.cancelled(event)
          dragWatch?.stop()
          dragWatch = null
          const newValue = Array.from(ele.children)
              .filter(row => row.hasAttribute('data-risu-idx2'))
              .map(row => value[Number(row.getAttribute('data-risu-idx2'))])
          // Restore Svelte's DOM before applying the new order.
          event.from.insertBefore(event.item, originalNextSibling)
          // An abandoned drag changes nothing.
          if (cancelled) return
          value = newValue
      },
      ...sortableOptions,
    })
  }

  const onOpen = (item: triggerscript) => {
      if (opened.has(item)) return
      opened = new Set([...opened, item])
      if (stb) {
          abandonSortDrag()
          stb.destroy()
          stb = null
      }
  }
  const onClose = (item: triggerscript) => {
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
    if (stb) {
      try {
        destroySortableOnClose(stb)
      } catch (error) {}
    }
  })
</script>

  <div
    class="contain w-full max-w-full mt-2 flex flex-col border-selected border-1 bg-darkbg rounded-md p-3"
    bind:this={ele}
  >
    {#if value.length === 0}
      <div class="text-textcolor2">No Scripts</div>
    {/if}
    {#each value as triggerscript, i (triggerscript)}
      <TriggerData
        idx={i}
        bind:value={value[i]}
        {lowLevelAble}
        onOpen={() => onOpen(triggerscript)}
        onClose={() => onClose(triggerscript)}
        onRemove={() => {
          let triggerscript = value
          triggerscript.splice(i, 1)
          value = triggerscript
        }}
      />
    {/each}
  </div>
  <button
    class="font-medium cursor-pointer hover:text-textcolor mb-2 text-textcolor2"
    onclick={() => {
      value.push({
        comment: '',
        type: 'start',
        conditions: [],
        effect: [],
      })
      value = value
    }}
  >
    <PlusIcon />
  </button>
