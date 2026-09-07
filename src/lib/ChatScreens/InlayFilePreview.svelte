<script lang="ts">
    import { getInlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
    import type { InlayRenderSource } from 'src/ts/process/files/inlayRenderSource'
    import { isTauri } from 'src/ts/platform'
    import { language } from 'src/lang'
    import { loadMediaSource } from '../UI/mediaSource'

    interface Props {
        id: string
    }

    let { id }: Props = $props()
    let source: InlayRenderSource | null = $state(null)
    let descriptor: InlayRenderSource | null = $state(null)
    let previewRoot: HTMLDivElement | null = $state(null)
    let unavailable = $state(false)
    let visible = $state(typeof IntersectionObserver === 'undefined')
    let playing = $state(false)
    const shouldLoad = $derived(visible || playing)
    const placeholderStyle = $derived.by(() => {
        if (descriptor?.type === 'audio') return 'width: 192px; height: 96px'
        const width = descriptor?.width
        const height = descriptor?.height
        if (!width || !height) return 'width: 192px; height: 192px'
        const scale = Math.min(1, 192 / width, 192 / height)
        return `width: ${Math.max(1, Math.round(width * scale))}px; height: ${Math.max(1, Math.round(height * scale))}px; aspect-ratio: ${width} / ${height}`
    })

    const unloadMedia = () => {
        const media = previewRoot?.querySelectorAll<HTMLMediaElement>('audio, video') ?? []
        for (const element of media) {
            if (!element.paused && !element.ended) element.pause()
            for (const child of element.querySelectorAll('source')) child.removeAttribute('src')
            element.load()
        }
    }

    const markPlaying = () => {
        playing = true
    }

    const markStopped = () => {
        playing = false
    }

    $effect(() => {
        id
        descriptor = null
        unavailable = false
        playing = false
    })

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
        const load = shouldLoad
        let disposed = false
        let objectUrl: string | null = null
        source = null
        if (!load) {
            unloadMedia()
            return
        }
        unavailable = false
        void getInlayRenderSource(assetId, isTauri).then((nextSource) => {
            if (disposed) {
                if (nextSource?.objectUrl) URL.revokeObjectURL(nextSource.url)
                return
            }
            source = nextSource
            if (nextSource) descriptor = { ...nextSource, url: '', objectUrl: false }
            // A stored attachment that cannot be resolved (never synced from
            // another device, or deleted) must say so instead of leaving an
            // empty box that looks like a stuck loading state.
            else unavailable = true
            objectUrl = nextSource?.objectUrl ? nextSource.url : null
        }, (error) => {
            console.error('Inlay preview failed', error)
            if (!disposed) unavailable = true
        })
        return () => {
            disposed = true
            unloadMedia()
            if (objectUrl) URL.revokeObjectURL(objectUrl)
        }
    })
</script>

<div bind:this={previewRoot} data-inlay-file-preview>
    <div data-inlay-file-preview-box style={placeholderStyle}>
        {#if unavailable}
            <div class="flex h-full w-full flex-col items-center justify-center gap-1 border border-darkborderc p-2 text-center text-xs text-textcolor2">
                <span>{language.inlayUnavailable}</span>
                <span class="w-full break-all">{id}</span>
            </div>
        {:else if descriptor?.type === 'image'}
            <img src={source?.url} alt="Inlay" class="w-full h-full object-contain border border-darkborderc">
        {:else if descriptor?.type === 'video'}
            <video controls class="w-full h-full border border-darkborderc" onplay={markPlaying} onpause={markStopped} onended={markStopped}>
                <source use:loadMediaSource={source?.url} type={descriptor.mime} />
                <track kind="captions" />
                Your browser does not support the video tag.
            </video>
        {:else if descriptor?.type === 'audio'}
            <audio controls class="w-full max-h-24 border border-darkborderc" onplay={markPlaying} onpause={markStopped} onended={markStopped}>
                <source use:loadMediaSource={source?.url} type={descriptor.mime} />
                Your browser does not support the audio tag.
            </audio>
        {:else if descriptor}
            <div class="max-w-24 max-h-24">{id}</div>
        {/if}
    </div>
</div>
