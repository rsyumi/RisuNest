<script lang="ts">
  import { onDestroy } from 'svelte'
  import { SvelteSet } from 'svelte/reactivity'

  import { language } from 'src/lang'
  import { alertConfirm, alertNormal } from 'src/ts/alert'
  import { getInlayEncodeOptions, listInlayAssetMetadata, removeInlayAsset } from 'src/ts/process/files/inlays'
  import {
    emptyInlayOptimizationProgress,
    runInlayOptimization,
    selectInlayOptimizationTargets,
    type InlayOptimizationProgress,
  } from 'src/ts/process/files/inlayOptimizationJob'
  import {
    inlayOptimizationConfirmMessage,
    inlayOptimizationProgressMessage,
    inlayOptimizationResultMessage,
  } from 'src/ts/process/files/inlayOptimizationMessages'
  import { summarizeInlayAssets } from 'src/ts/process/files/inlayInventory'
  import { getInlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
  import type { InlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
  import { isTauri } from 'src/ts/platform'
  import type { InlayBlobMetadata } from 'src/ts/storage/blobStore'
  import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'
  import Button from '../UI/GUI/Button.svelte'
  import CheckInput from '../UI/GUI/CheckInput.svelte'
  import { loadMediaSource } from '../UI/mediaSource'

  const PAGE_SIZE = 36

  let allAssets = $state<InlayBlobMetadata[]>([])
  let displayCount = $state(PAGE_SIZE)
  let loading = $state(true)
  let loadMoreSentinel: HTMLDivElement | null = $state(null)
  let previewSources = $state<Map<string, InlayRenderSource>>(new Map())
  const pendingPreviews = new Map<string, { generation: number; promise: Promise<string | null> }>()
  const previewGenerations = new Map<string, number>()
  let destroyed = false
  let selection = $state<Set<string>>(new SvelteSet())

  const displayedAssets = $derived(allAssets.slice(0, displayCount))
  const hasMore = $derived(displayCount < allAssets.length)
  const hasSelection = $derived(selection.size > 0)
  const inventory = $derived(summarizeInlayAssets(allAssets))
  let optimizing = $state(false)
  let optimizeProgress = $state<InlayOptimizationProgress>(emptyInlayOptimizationProgress())
  const extensionSummary = $derived(
    [...inventory.images, ...inventory.others]
      .map((entry) => `${entry.ext || language.risuNest.inlay.inventoryNoExtension} ${entry.count.toLocaleString()}`)
      .join(' · '),
  )

  const getPreviewURL = async (asset: InlayBlobMetadata) => {
    const id = asset.key
    const cached = previewSources.get(id)
    if (cached) return cached.url
    const generation = previewGenerations.get(id) ?? 0
    const existing = pendingPreviews.get(id)
    if (existing?.generation === generation) return existing.promise
    let pending: Promise<string | null>
    pending = (async () => {
      const source = await getInlayRenderSource(id, isTauri, asset)
      if (!source) return null
      if (destroyed || (previewGenerations.get(id) ?? 0) !== generation) {
        if (source.objectUrl) URL.revokeObjectURL(source.url)
        return null
      }
      previewSources = new Map(previewSources).set(id, source)
      return source.url
    })()
      .catch(() => null)
      .finally(() => {
        if (pendingPreviews.get(id)?.promise === pending) {
          pendingPreviews.delete(id)
        }
      })
    pendingPreviews.set(id, { generation, promise: pending })
    return pending
  }

  const removePreview = (id: string) => {
    previewGenerations.set(id, (previewGenerations.get(id) ?? 0) + 1)
    const source = previewSources.get(id)
    if (source?.objectUrl) URL.revokeObjectURL(source.url)
    if (previewSources.delete(id)) previewSources = new Map(previewSources)
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

  const optimizeSelected = async () => {
    if (selection.size === 0 || optimizing) return
    const options = getInlayEncodeOptions()
    const targets = selectInlayOptimizationTargets(
      allAssets.filter((asset) => selection.has(asset.key)),
      options,
    )
    if (targets.length === 0) {
      alertNormal(language.risuNest.inlay.optimizeNone)
      return
    }
    // Loaded here so the playground does not pull the storage and sync wiring on open.
    const runtime = await import('src/ts/process/files/inlayOptimizationRuntime')
    const message = inlayOptimizationConfirmMessage({
      targets,
      storedFormat: options.format,
      ...(await runtime.readInlayOptimizationEnvironment()),
    })
    if (!(await alertConfirm(message))) return
    optimizing = true
    optimizeProgress = emptyInlayOptimizationProgress(targets.length)
    try {
      const done = await runInlayOptimization(targets, runtime.createStoredInlayOptimizationDeps(), {
        options,
        onProgress: (value) => {
          optimizeProgress = value
        },
      })
      selection.clear()
      await loadAssets()
      alertNormal(inlayOptimizationResultMessage(done))
    } finally {
      optimizing = false
    }
  }

  const formatSize = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  }

  const previewTargets = new Map<Element, InlayBlobMetadata>()
  const visiblePreviewTargets = new Set<Element>()
  const playingPreviewIds = new Set<string>()
  let previewObserver: IntersectionObserver | null = null

  const isPreviewVisible = (id: string) => {
    for (const target of visiblePreviewTargets) {
      if (previewTargets.get(target)?.key === id) return true
    }
    return false
  }

  const markPreviewPlaying = (id: string) => {
    playingPreviewIds.add(id)
  }

  const markPreviewStopped = (id: string) => {
    playingPreviewIds.delete(id)
    if (!isPreviewVisible(id)) removePreview(id)
  }

  const ensurePreviewObserver = () => {
    if (previewObserver || typeof IntersectionObserver === 'undefined') return previewObserver
    previewObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          const asset = previewTargets.get(entry.target)
          if (!asset) continue
          if (entry.isIntersecting) {
            visiblePreviewTargets.add(entry.target)
            void getPreviewURL(asset)
          } else {
            visiblePreviewTargets.delete(entry.target)
            if (!playingPreviewIds.has(asset.key)) removePreview(asset.key)
          }
        }
      },
      { root: null, rootMargin: '256px 0px', threshold: 0 }
    )
    return previewObserver
  }

  const observePreview = (node: HTMLElement, asset: InlayBlobMetadata) => {
    let current = asset
    const attach = () => {
      previewTargets.set(node, current)
      const currentObserver = ensurePreviewObserver()
      if (currentObserver) currentObserver.observe(node)
      else {
        visiblePreviewTargets.add(node)
        void getPreviewURL(current)
      }
    }
    const detach = () => {
      previewObserver?.unobserve(node)
      previewTargets.delete(node)
      visiblePreviewTargets.delete(node)
      playingPreviewIds.delete(current.key)
      removePreview(current.key)
    }
    attach()
    return {
      update(next: InlayBlobMetadata) {
        if (next.key === current.key) {
          current = next
          previewTargets.set(node, current)
          if (visiblePreviewTargets.has(node) && !previewSources.has(current.key) && !pendingPreviews.has(current.key)) {
            void getPreviewURL(current)
          }
          return
        }
        detach()
        current = next
        attach()
      },
      destroy: detach,
    }
  }

  let loadMoreObserver: IntersectionObserver | null = null
  $effect(() => {
    if (!loadMoreSentinel || !hasMore) {
      loadMoreObserver?.disconnect()
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

    loadMoreObserver?.disconnect()
    loadMoreObserver = new IntersectionObserver(
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
    loadMoreObserver.observe(loadMoreSentinel)

    return () => {
      loadMoreObserver?.disconnect()
      loadMoreObserver = null
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
    previewObserver?.disconnect()
    previewObserver = null
    previewTargets.clear()
    visiblePreviewTargets.clear()
    playingPreviewIds.clear()
    loadMoreObserver?.disconnect()
  })

  const loadAssets = async () => {
    loading = true
    allAssets = await listInlayAssetMetadata()
    loading = false
  }
  loadAssets()
</script>

<h2 class="text-4xl text-textcolor mt-6 font-black relative">{language.playground.inlayExplorer}</h2>

<header class="flex flex-wrap gap-4 py-6 items-center sticky top-0 bg-bgcolor">
  <span class="text-textcolor2">{language.playground.inlayTotalAssets.replace('{count}', allAssets.length.toString())}</span>
  {#if allAssets.length > 0}
    <span data-inlay-extension-summary class="text-textcolor2 min-w-0 text-sm break-words"
      >{formatRisuNestStorageBytes(inventory.total.bytes)} · {extensionSummary}</span
    >
    <div class="flex gap-2 ml-auto">
      {#if hasSelection}
        {#if optimizing}
          <span class="text-textcolor2 self-center text-sm tabular-nums" role="status" aria-live="polite"
            >{inlayOptimizationProgressMessage(optimizeProgress)}</span
          >
        {:else}
          <Button onclick={optimizeSelected} styled="primary" size="sm">{language.risuNest.inlay.optimizeSelected}</Button>
        {/if}
        <Button onclick={deleteSelected} styled="danger" size="sm" disabled={optimizing}>{language.playground.inlayDeleteSelected}</Button>
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
      <div
        class="border border-darkborderc rounded-lg p-4 bg-darkbg"
        data-inlay-preview-id={asset.key}
        use:observePreview={asset}
      >
          <div class="flex items-center gap-2 mb-3">
            <CheckInput check={selection.has(asset.key)} hiddenName margin={false} onChange={() => toggleSelect(asset.key)} />
            <span class="px-2 py-1 text-xs rounded bg-darkbutton text-textcolor2">
              {asset.inlayType}
            </span>
          </div>
          <div class="mb-3">
            {#if asset.inlayType === 'image'}
              <img alt={asset.name} class="w-full h-40 object-contain rounded bg-black/20" src={previewSources.get(asset.key)?.url} width={asset.width} height={asset.height} />
            {:else if asset.inlayType === 'video'}
              <video class="w-full h-40 object-contain rounded bg-black/20" controls onplay={() => markPreviewPlaying(asset.key)} onpause={() => markPreviewStopped(asset.key)} onended={() => markPreviewStopped(asset.key)}>
                <source use:loadMediaSource={previewSources.get(asset.key)?.url} type={asset.mime} />
                <track kind="captions" />
              </video>
            {:else if asset.inlayType === 'audio'}
              <audio class="w-full min-h-12" controls onplay={() => markPreviewPlaying(asset.key)} onpause={() => markPreviewStopped(asset.key)} onended={() => markPreviewStopped(asset.key)}>
                <source use:loadMediaSource={previewSources.get(asset.key)?.url} type={asset.mime} />
                <track kind="captions" />
              </audio>
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
    {/each}
  </div>

  {#if hasMore}
    <div bind:this={loadMoreSentinel} class="h-12 flex items-center justify-center text-textcolor2 text-sm">
      Loading...
    </div>
  {/if}
{/if}
