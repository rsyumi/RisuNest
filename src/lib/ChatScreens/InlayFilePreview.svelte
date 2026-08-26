<script lang="ts">
    import { getInlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
    import type { InlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
    import { isTauri } from 'src/ts/platform'

    interface Props {
        id: string
    }

    let { id }: Props = $props()
    let source: InlayRenderSource | null = $state(null)
    let previewRoot: HTMLDivElement | null = $state(null)
    let visible = $state(typeof IntersectionObserver === 'undefined')

    const unloadMedia = () => {
        const media = previewRoot?.querySelectorAll<HTMLMediaElement>('audio, video') ?? []
        for (const element of media) {
            element.pause()
            for (const child of element.querySelectorAll('source')) child.removeAttribute('src')
            element.load()
        }
    }

    $effect(() => {
        const root = previewRoot
        if (!root) return
        if (typeof IntersectionObserver === 'undefined') {
            visible = true
            return
        }
        visible = false
        const observer = new IntersectionObserver(
            (entries) => {
                visible = entries[0]?.isIntersecting ?? false
            },
            { root: null, rootMargin: '256px 0px', threshold: 0 },
        )
        observer.observe(root)
        return () => observer.disconnect()
    })

    $effect(() => {
        const assetId = id
        const shouldLoad = visible
        let disposed = false
        let objectUrl: string | null = null
        source = null
        if (!shouldLoad) {
            unloadMedia()
            return
        }
        void getInlayRenderSource(assetId, isTauri).then((nextSource) => {
            if (disposed) {
                if (nextSource?.objectUrl) URL.revokeObjectURL(nextSource.url)
                return
            }
            source = nextSource
            objectUrl = nextSource?.objectUrl ? nextSource.url : null
        })
        return () => {
            disposed = true
            unloadMedia()
            if (objectUrl) URL.revokeObjectURL(objectUrl)
        }
    })
</script>

<div bind:this={previewRoot} data-inlay-file-preview>
    {#if source?.type === 'image'}
        <img src={source.url} alt="Inlay" class="max-w-48 max-h-48 border border-darkborderc">
    {:else if source?.type === 'video'}
        <video controls class="max-w-48 max-h-48 border border-darkborderc">
            <source src={source.url} type={source.mime} />
            <track kind="captions" />
            Your browser does not support the video tag.
        </video>
    {:else if source?.type === 'audio'}
        <audio controls class="max-w-48 max-h-24 border border-darkborderc">
            <source src={source.url} type={source.mime} />
            Your browser does not support the audio tag.
        </audio>
    {:else if source}
        <div class="max-w-24 max-h-24">{id}</div>
    {/if}
</div>
