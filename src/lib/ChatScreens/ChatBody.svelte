<script lang="ts">
    import isEqual from "lodash/isEqual"
    import { DBState } from 'src/ts/stores.svelte'
    import { sleep } from "src/ts/util"
    import { alertError } from "../../ts/alert"
    import { addMetadataToElement, getDistance, ParseMarkdown, postTranslationParse, trimMarkdown, type CbsConditions, type simpleCharacterArgument } from "../../ts/parser/parser.svelte"
    import { getLLMCache, translateHTML } from "../../ts/translator/translator"
    import { getModuleAssets } from "src/ts/process/modules";
    import { getCurrentCharacter } from "src/ts/storage/database.svelte";
    import { getFileSrc } from "src/ts/globalApi.svelte";
    import { DeferredInlayMarkerRegistry, mountDeferredInlaySources, resolveDeferredInlaySources } from "src/ts/process/files/inlayRenderSource";
    import { onDestroy, tick } from 'svelte'

    interface Props {
        character?: simpleCharacterArgument|string|null
        firstMessage?: boolean
        idx?: number
        msgDisplay?: string
        name?: string
        role: string|null
        translated: boolean
        translating: boolean
        retranslate: boolean
        bodyRoot?: HTMLElement|null
        modelShortName: string
        renderRawStreaming?: boolean
        rawStreamingText?: string
        onCaptureSettled?: (generation: number) => void
    }

    let {
        character = null,
        idx = 0,
        firstMessage = false,
        msgDisplay,
        role,
        translated = $bindable(false),
        translating = $bindable(false),
        retranslate = $bindable(false),
        bodyRoot,
        modelShortName = '',
        renderRawStreaming = false,
        rawStreamingText = '',
        onCaptureSettled,
    }: Props =  $props()

    // svelte-ignore non_reactive_update
    let lastParsed = ''
    let lastCharArg:string|simpleCharacterArgument = null
    let lastChatId = -10
    let renderRoot = $state<HTMLElement | undefined>(undefined)
    let releaseObjectUrls = () => {}
    let destroyed = false

    interface ChatBodyParseJob {
        promise: Promise<string>
        deferredInlays: DeferredInlayMarkerRegistry
        disposed: boolean
        generation: number
        settledNotified: boolean
    }

    let activeParseJob: ChatBodyParseJob|null = null
    let parseGeneration = 0

    function getCbsCondition(){
        try{
            const cbsConditions:CbsConditions = {
                firstmsg: firstMessage ?? false,
                chatRole: role,
            }
            return cbsConditions
        }
        catch(e){
            return {
                firstmsg: firstMessage ?? false,
                chatRole: null,
            }
        }
    }

    let shouldRenderRawStreaming = $derived(renderRawStreaming && !translated && !retranslate)

    const markParsing = async (data: string, charArg: string | simpleCharacterArgument, chatID: number, job:ChatBodyParseJob, tries?:number):Promise<string> => {
        const parseForRender = (value:string, mode:'normal'|'back'|'pretranslate'|'notrim') => (
            ParseMarkdown(value, charArg, mode, chatID, getCbsCondition(), { deferredInlays: job.deferredInlays })
        )
        // track 'translated' and 'retranslate' state
        translated;
        retranslate;
        let lastParsedQueue = ''
        let mode = 'notrim' as const
        try {
            if((!isEqual(lastCharArg, charArg)) || (chatID !== lastChatId)){
                lastParsedQueue = ''
                lastCharArg = charArg
                lastChatId = chatID
                let translateText = false
                try {
                    if(DBState.db.autoTranslate){
                        if(DBState.db.autoTranslateCachedOnly && DBState.db.translatorType === 'llm'){
                            const cache = DBState.db.translateBeforeHTMLFormatting
                            ? await getLLMCache(data)
                            : !DBState.db.legacyTranslation
                            ? await getLLMCache(await ParseMarkdown(data, charArg, 'pretranslate', chatID, getCbsCondition()))
                            : await getLLMCache(await ParseMarkdown(data, charArg, mode, chatID, getCbsCondition()))
                  
                            translateText = cache !== null
                        }
                        else{
                            translateText = true
                        }
                    }

                    const lastTranslated = translated

                    setTimeout(() => {
                            translated = translateText
                    }, 10)

                    // State change of `translated` triggers markParsing again,
                    // causing redundant translation attempts
                    if (lastTranslated !== translateText) {
                        return ''
                    }
                } catch (error) {
                    console.error(error)
                }
            }
            if(retranslate || translated){
                if (DBState.db.showTranslationLoading) {
                    lastParsed = `<div style="display:flex;justify-content:center;align-items:center;height:48px;"><div style="animation: spin 1s linear infinite; border-radius: 50%; height: 32px; width: 32px; border: 2px solid #3b82f6; border-top: 2px solid transparent;"></div></div><style>@keyframes spin { to { transform: rotate(360deg); } }</style>`
                }

                let transResult
                
                if(DBState.db.translatorType === 'llm' && DBState.db.translateBeforeHTMLFormatting){
                    await sleep(100)
                    translating = true
                    data = await translateHTML(data, false, charArg, chatID, retranslate)
                    translating = false
                    const marked = await parseForRender(data, mode)
                    lastParsedQueue = marked
                    lastCharArg = charArg
                    transResult = marked
                }
                else if(!DBState.db.legacyTranslation){
                    const marked = await parseForRender(data, 'pretranslate')
                    translating = true
                    const translated = await postTranslationParse(await translateHTML(marked, false, charArg, chatID, retranslate))
                    translating = false
                    lastParsedQueue = translated
                    lastCharArg = charArg
                    transResult = translated
                }
                else{
                    const marked = await parseForRender(data, mode)
                    translating = true
                    const translated = await translateHTML(marked, false, charArg, chatID, retranslate)
                    translating = false
                    lastParsedQueue = translated
                    lastCharArg = charArg
                    transResult = translated
                }

                setTimeout(() => {
                    retranslate = false
                }, 10);

                return transResult
            }
            else{
                const marked = await parseForRender(data, mode)
                lastParsedQueue = marked
                lastCharArg = charArg
                return marked
            }   
        } catch (error) {
            //retry
            if(tries > 2){

                alertError(`Error while parsing chat message: ${translated}, ${error.message}, ${error.stack}`)
                return data
            }
            if(job.disposed) return data
            job.deferredInlays.clear()
            job.deferredInlays = new DeferredInlayMarkerRegistry()
            return await markParsing(data, charArg, chatID, job, (tries ?? 0) + 1)
        }
        finally{
            //since trimMarkdown is fast, we don't need to cache it
            lastParsed = lastParsedQueue
        }
    }

    const checkImg = async (job: ChatBodyParseJob) => {
        if(!DBState.db.newImageHandlingBeta || !bodyRoot){
            return
        }
        const imgs = bodyRoot.querySelectorAll('img:not([src^="data:"]):not([src^="http:"]):not([src^="https:"]):not([src^="blob:"]):not([src^="file:"]):not([src^="tauri:"]):not([noimage])') as NodeListOf<HTMLImageElement>
        
        if (imgs.length > 0) {
            const currentCharacter = getCurrentCharacter()
            const styl = currentCharacter.prebuiltAssetStyle
            const assets = getModuleAssets().concat(currentCharacter.additionalAssets ?? [])
            const normalizedAssets = assets.map((asset) => {
                return {
                    name: asset[0].toLocaleLowerCase(),
                    path: asset[1]
                }
            })
            const exactAssets = new Map(normalizedAssets.map((asset) => [asset.name, asset.path]))

            await Promise.all(Array.from(imgs).map(async (img) => {
                const name = img.getAttribute('src')?.toLocaleLowerCase() || ''
                console.log(name)

                if(
                    name.length > 200 ||
                    name.includes(':')
                ){
                    img.setAttribute('noimage', 'true')
                    return
                }
                
                const foundAsset = exactAssets.get(name)
                console.log('Checking image:', name, 'Assets:', assets)
                if(foundAsset){
                    img.classList.add('root-loaded-image')
                    img.classList.add('root-loaded-image-' + styl)
                    const source = await getFileSrc(foundAsset)
                    if (destroyed || job.disposed || job !== activeParseJob) return
                    img.src = source
                    return
                }

                if(name.length < 3){
                    img.setAttribute('noimage', 'true')
                    return
                }
                const prefixLoc = name.lastIndexOf('.')
                const prefix = prefixLoc > 0 ? name.substring(0, prefixLoc) : ''
                let currentDistance = 1000
                let currentFound = ''
                for(const asset of normalizedAssets){
                    if(!asset.name.startsWith(prefix)){
                        continue
                    }
                    const distance = getDistance(name, asset.name)
                    if(distance < currentDistance){
                        currentDistance = distance
                        currentFound = asset.path
                    }
                }
                if(currentFound){
                    const got = await getFileSrc(currentFound)
                    if (destroyed || job.disposed || job !== activeParseJob) return
                    const name2 = img.getAttribute('src')?.toLocaleLowerCase() || ''
                    if(name === name2){
                        img.setAttribute('src', got)
                    }

                    if(img.classList.length === 0){
                        img.classList.add('root-loaded-image')
                        img.classList.add('root-loaded-image-' + styl)
                    }
                    img.removeAttribute('noimage')
                }
                else{
                    img.setAttribute('noimage', 'true')
                }
            }))
        }
    }

    function startParsing():ChatBodyParseJob {
        const job:ChatBodyParseJob = {
            promise: Promise.resolve(''),
            deferredInlays: new DeferredInlayMarkerRegistry(),
            disposed: false,
            generation: ++parseGeneration,
            settledNotified: false,
        }
        job.promise = markParsing(msgDisplay, character, idx, job)
        return job
    }

    function disposeParseJob(job:ChatBodyParseJob|null) {
        if (!job || job.disposed) return
        job.disposed = true
        job.deferredInlays.clear()
    }

    let markParsingResult = $derived.by(() => shouldRenderRawStreaming ? null : startParsing())

    async function syncObjectUrls(job: ChatBodyParseJob) {
        try {
            await job.promise
            if (destroyed || job.disposed || job !== markParsingResult) {
                disposeParseJob(job)
                return
            }
            await tick()
            if (destroyed || job.disposed || job !== markParsingResult) {
                disposeParseJob(job)
                return
            }
            releaseObjectUrls()
            if (renderRoot) {
                releaseObjectUrls = onCaptureSettled
                    ? await resolveDeferredInlaySources(renderRoot, job.deferredInlays)
                    : mountDeferredInlaySources(renderRoot, job.deferredInlays)
            }
            else disposeParseJob(job)
            if (destroyed || job.disposed || job !== activeParseJob) {
                releaseObjectUrls()
                releaseObjectUrls = () => {}
                return
            }
            await checkImg(job)
            await tick()
            if (destroyed || job.disposed || job !== activeParseJob || job.settledNotified) return
            job.settledNotified = true
            onCaptureSettled?.(job.generation)
        }
        catch {
            // markParsing handles its own failures
        }
    }

    onDestroy(() => {
        destroyed = true
        releaseObjectUrls()
        disposeParseJob(activeParseJob)
    })

    $effect(() => {
        const result = markParsingResult
        if (activeParseJob !== result) {
            disposeParseJob(activeParseJob)
            activeParseJob = result
        }
        if(shouldRenderRawStreaming){
            releaseObjectUrls()
            releaseObjectUrls = () => {}
            return
        }
        if (!result) return
        void syncObjectUrls(result)
    })
</script>

{#if shouldRenderRawStreaming}
    <span class="whitespace-pre-wrap">{rawStreamingText}</span>
{:else}
    <span style="display:contents" bind:this={renderRoot}>
        {#await markParsingResult?.promise}
            {@html addMetadataToElement(trimMarkdown(lastParsed), modelShortName)}
        {:then parsed}
            {@html addMetadataToElement(trimMarkdown(parsed ?? ''), modelShortName)}
        {/await}
    </span>
{/if}
