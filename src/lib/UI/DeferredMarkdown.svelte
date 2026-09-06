<script lang="ts">
    import { onDestroy, tick } from 'svelte'
    import { ParseMarkdown } from 'src/ts/parser/parser.svelte'
    import {
        DeferredInlayMarkerRegistry,
        mountDeferredInlaySources,
    } from 'src/ts/process/files/inlayRenderSource'

    type MarkdownCharacter = Parameters<typeof ParseMarkdown>[1]
    type MarkdownMode = Parameters<typeof ParseMarkdown>[2]
    type MarkdownConditions = Parameters<typeof ParseMarkdown>[4]

    interface Props {
        data: string
        character?: MarkdownCharacter
        mode?: MarkdownMode
        chatID?: number
        conditions?: MarkdownConditions
    }

    let {
        data,
        character = null,
        mode = 'normal',
        chatID = -1,
        conditions = {},
    }: Props = $props()
    let root = $state<HTMLElement>()
    let releaseObjectUrls = () => {}
    let activeRegistry: DeferredInlayMarkerRegistry | null = null
    let destroyed = false

    function startParsing() {
        const registry = new DeferredInlayMarkerRegistry()
        return {
            registry,
            promise: ParseMarkdown(data, character, mode, chatID, conditions, { deferredInlays: registry }),
        }
    }

    const parseJob = $derived.by(startParsing)

    async function mountSources(job: ReturnType<typeof startParsing>) {
        await job.promise
        if (destroyed || job !== parseJob) {
            job.registry.clear()
            return
        }
        await tick()
        if (destroyed || job !== parseJob) {
            job.registry.clear()
            return
        }
        releaseObjectUrls()
        if (root) releaseObjectUrls = mountDeferredInlaySources(root, job.registry)
        else job.registry.clear()
    }

    $effect(() => {
        const job = parseJob
        if (activeRegistry !== job.registry) {
            activeRegistry?.clear()
            activeRegistry = job.registry
        }
        // A failed parse still has to release the job's registry; letting the
        // rejection escape leaks it and reports an unhandled rejection.
        void mountSources(job).catch((error) => {
            job.registry.clear()
            console.error('Deferred markdown render failed', error)
        })
    })

    onDestroy(() => {
        destroyed = true
        releaseObjectUrls()
        activeRegistry?.clear()
    })
</script>

<span style="display:contents" bind:this={root}>
    {#await parseJob.promise then html}
        {@html html}
    {/await}
</span>
