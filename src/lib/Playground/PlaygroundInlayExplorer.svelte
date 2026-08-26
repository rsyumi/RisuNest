<script lang="ts">
  import { onDestroy } from 'svelte'
  import { SvelteSet } from 'svelte/reactivity'

  import { language } from 'src/lang'
  import { alertConfirm } from 'src/ts/alert'
  import { listInlayAssetMetadata, removeInlayAsset } from 'src/ts/process/files/inlays'
  import { getInlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
  import type { InlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
  import { isTauri } from 'src/ts/platform'
  import type { InlayBlobMetadata } from 'src/ts/storage/blobStore'
  import Button from '../UI/GUI/Button.svelte'
  import CheckInput from '../UI/GUI/CheckInput.svelte'

  const PAGE_SIZE = 36

  let allAssets = $state<InlayBlobMetadata[]>([])
  let displayCount = $state(PAGE_SIZE)
  let loading = $state(true)
  let loadMoreSentinel: HTMLDivElement | null = $state(null)
  let previewSources = $state<Map<string, InlayRenderSource>>(new Map())
  const pendingPreviews = new Map<string, Promise<string | null>>()
  const previewGenerations = new Map<string, number>()
  let destroyed = false
  let selection = $state<Set<string>>(new SvelteSet())

  const displayedAssets = $derived(allAssets.slice(0, displayCount))
  const hasMore = $derived(displayCount < allAssets.length)
  const hasSelection = $derived(selection.size > 0)

  const getPreviewURL = async (asset: InlayBlobMetadata) => {
    const id = asset.key
    const cached = previewSources.get(id)
    if (cached) return cached.url
    const existing = pendingPreviews.get(id)
    if (existing) return existing
    const generation = previewGenerations.get(id) ?? 0
    let pending: Promise<string | null>
    pending = (async () => {
      const source = await getInlayRenderSource(id, isTauri, asset)
      if (!source) return null
      if (destroyed || (previewGenerations.get(id) ?? 0) !== generation) {
        if (source.objectUrl) URL.revokeObjectURL(source.url)
        return null
      }
      previewSources.set(id, source)
      return source.url
    })()
      .catch(() => null)
      .finally(() => {
        if (pendingPreviews.get(id) === pending) {
          pendingPreviews.delete(id)
        }
      })
    pendingPreviews.set(id, pending)
    return pending
  }

  const removePreview = (id: string) => {
    previewGenerations.set(id, (previewGenerations.get(id) ?? 0) + 1)
    const source = previewSources.get(id)
    if (source?.objectUrl) URL.revokeObjectURL(source.url)
    previewSources.delete(id)
  }

  const toggleSelect = (id: string) => {
    if (selection.has(id)) {
      selection.delete(id)
    } else {
      selection.add(id)
    }
  }

  const selectAll = () => {
    displayedAssets.forEach((asset) => selection.add(asset.key))
  }

  const deselectAll = () => {
    selection.clear()
  }

  const deleteAsset = async (id: string, name: string) => {
    if (!(await alertConfirm(language.playground.inlayDeleteConfirm.replace('{name}', name)))) {
      return
    }
    await removeInlayAsset(id)
    removePreview(id)
    selection.delete(id)
    allAssets = allAssets.filter((asset) => asset.key !== id)
  }

  const deleteSelected = async () => {
    if (selection.size === 0) return
    if (!(await alertConfirm(language.playground.inlayDeleteMultipleConfirm.replace('{count}', selection.size.toString())))) {
      return
    }
    for (const id of selection) {
      await removeInlayAsset(id)
      removePreview(id)
    }
    allAssets = allAssets.filter((asset) => !selection.has(asset.key))
    selection.clear()
  }

  const formatSize = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  }

  let observer: IntersectionObserver | null = null
  $effect(() => {
    if (!loadMoreSentinel || !hasMore) {
      observer?.disconnect()
      return
    }

    const loadMore = () => {
      if (!hasMore || loading) {
        return
      }

      loading = true
      displayCount += PAGE_SIZE
      queueMicrotask(() => {
        loading = false
      })
    }

    observer?.disconnect()
    observer = new IntersectionObserver(
      (entries) => {
        if (entries[0]?.isIntersecting) {
          loadMore()
        }
      },
      {
        root: null,
        rootMargin: '200px 0px',
        threshold: 0,
      }
    )
    observer.observe(loadMoreSentinel)

    return () => {
      observer?.disconnect()
      observer = null
    }
  })

  onDestroy(() => {
    destroyed = true
    previewSources.forEach((source) => {
      if (source.objectUrl) URL.revokeObjectURL(source.url)
    })
    previewSources.clear()
    pendingPreviews.clear()
    previewGenerations.clear()
    observer?.disconnect()
  })

  const loadAssets = async () => {
    loading = true
    allAssets = await listInlayAssetMetadata({ migrateLegacy: !isTauri })
    loading = false
  }
  loadAssets()
</script>

<h2 class="text-4xl text-textcolor mt-6 font-black relative">{language.playground.inlayExplorer}</h2>

<header class="flex flex-wrap gap-4 py-6 items-center sticky top-0 bg-bgcolor">
  <span class="text-textcolor2">{language.playground.inlayTotalAssets.replace('{count}', allAssets.length.toString())}</span>
  {#if allAssets.length > 0}
    <div class="flex gap-2 ml-auto">
      {#if hasSelection}
        <Button onclick={deleteSelected} styled="danger" size="sm">{language.playground.inlayDeleteSelected}</Button>
        <Button onclick={deselectAll} styled="primary" size="sm"
          >{language.playground.inlayDeselectAll} ({selection.size})</Button
        >
      {:else}
        <Button onclick={selectAll} styled="primary" size="sm">{language.playground.inlaySelectAll}</Button>
      {/if}
    </div>
  {/if}
</header>

{#if allAssets.length === 0 && !loading}
  <div class="text-center py-12 text-textcolor2">
    <p class="text-lg">{language.playground.inlayEmpty}</p>
    <p class="text-sm mt-2">{language.playground.inlayEmptyDesc}</p>
  </div>
{:else}
  <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
    {#each displayedAssets as asset (asset.key)}
      {#key selection.has(asset.key)}
        <div class="border border-darkborderc rounded-lg p-4 bg-darkbg">
          <div class="flex items-center gap-2 mb-3">
            <CheckInput check={selection.has(asset.key)} hiddenName margin={false} onChange={() => toggleSelect(asset.key)} />
            <span class="px-2 py-1 text-xs rounded bg-darkbutton text-textcolor2">
              {asset.inlayType}
            </span>
          </div>
          <div class="mb-3">
            {#if asset.inlayType === 'image'}
              {#await getPreviewURL(asset) then url}
                {#if url}
                  <img alt={asset.name} class="w-full h-40 object-contain rounded bg-black/20" src={url} />
                {/if}
              {/await}
            {:else if asset.inlayType === 'video'}
              {#await getPreviewURL(asset) then url}
                {#if url}
                  <video class="w-full h-40 object-contain rounded bg-black/20" controls>
                    <source src={url} type={asset.mime} />
                    <track kind="captions" />
                  </video>
                {/if}
              {/await}
            {:else if asset.inlayType === 'audio'}
              {#await getPreviewURL(asset) then url}
                {#if url}
                  <audio class="w-full" controls>
                    <source src={url} type={asset.mime} />
                    <track kind="captions" />
                  </audio>
                {/if}
              {/await}
            {/if}
          </div>

          <div class="flex justify-between items-start mb-2">
            <div class="flex-1 min-w-0">
              <p class="text-textcolor font-medium truncate" title={asset.name}>{asset.name}</p>
              {#if asset.name !== asset.key}
                <p class="text-textcolor2 text-xs truncate" title={asset.key}>{asset.key}</p>
              {/if}
            </div>
          </div>

          <div class="text-textcolor2 text-sm mb-3">
            {#if asset.width && asset.height}
              <span>{asset.width}x{asset.height} • </span>
            {/if}
            <span>{formatSize(asset.size)}</span>
          </div>

          <Button onclick={() => deleteAsset(asset.key, asset.name)} styled="danger" size="sm">Delete</Button>
        </div>
      {/key}
    {/each}
  </div>

  {#if hasMore}
    <div bind:this={loadMoreSentinel} class="h-12 flex items-center justify-center text-textcolor2 text-sm">
      Loading...
    </div>
  {/if}
{/if}
