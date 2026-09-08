<script lang="ts">
    import isEqual from "lodash/isEqual"
    import { DBState, selIdState } from 'src/ts/stores.svelte'
    import { sleep } from "src/ts/util"
    import { alertError } from "../../ts/alert"
    import { addMetadataToElement, getDistance, ParseMarkdown, postTranslationParse, trimMarkdown, type CbsConditions, type simpleCharacterArgument } from "../../ts/parser/parser.svelte"
    import { getLLMCache, translateHTML, type TranslateHTMLContext } from "../../ts/translator/translator"
    import { getModuleAssets } from "src/ts/process/modules";
    import { getCurrentCharacter } from "src/ts/storage/database.svelte";
    import { getFileSrc } from "src/ts/globalApi.svelte";
    import { DeferredInlayMarkerRegistry, mountDeferredInlaySources, resolveDeferredInlaySources } from "src/ts/process/files/inlayRenderSource";
    import { onDestroy, tick } from 'svelte'
    import type { FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { ProcessScriptCaptureContext } from 'src/ts/process/scripts'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type { character as CharacterRecord, groupChat as GroupChatRecord } from 'src/ts/storage/database.svelte'
    import { yieldToMainThread } from 'src/ts/ui/yieldToUi'

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
        renderRevision?: number
        bodyRoot?: HTMLElement|null
        modelShortName: string
        renderRawStreaming?: boolean
        rawStreamingText?: string
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
        captureContext?: FrozenChatScreenshotRenderContext
        captureParserIndex?: number
        parserProjection?: BoundedLiveChatParserProjection
    }

    let {
        character = null,
        idx = 0,
        firstMessage = false,
        msgDisplay,
        name = '',
        role,
        translated = $bindable(false),
        translating = $bindable(false),
        retranslate = $bindable(false),
        renderRevision = 0,
        bodyRoot,
        modelShortName = '',
        renderRawStreaming = false,
        rawStreamingText = '',
        onCaptureSettled,
        onCaptureError,
        captureContext,
        captureParserIndex = idx,
        parserProjection,
    }: Props =  $props()

    // svelte-ignore non_reactive_update
    let lastParsed = ''
    let lastCharArg:string|simpleCharacterArgument = null
    let lastChatId = -10
    let renderRoot = $state<HTMLElement | undefined>(undefined)
    let destroyed = false

    interface ChatBodyParseJob {
        promise: Promise<string>
        deferredInlays: DeferredInlayMarkerRegistry
        disposed: boolean
        generation: number
        requestedRevision: number
        preservePendingContent: boolean
        settledNotified: boolean
        errorNotified: boolean
        transitional: boolean
        releaseObjectUrls: () => void
    }

    let activeParseJob: ChatBodyParseJob|null = null
    let parseGeneration = 0
    let lastRenderedRevision: number | null = null

    function parserChara(): string | CharacterRecord | GroupChatRecord {
        const parserCharacter = captureContext?.parserContext.character
            ?? parserProjection?.context.parserContext.character
        const character = parserCharacter as CharacterRecord | GroupChatRecord
        return character?.type === 'group' ? name : character
    }

    function parserScriptContext(): ProcessScriptCaptureContext | undefined {
        if (parserProjection) {
            return {
                ...parserProjection.context,
                parserContext: {
                    ...parserProjection.context.parserContext,
                    chara: parserChara(),
                },
            }
        }
        if (!captureContext) return undefined
        return {
            presetRegex: captureContext.presetRegex,
            moduleRegexScripts: captureContext.moduleRegexScripts,
            moduleAssets: captureContext.moduleAssets,
            dynamicAssets: captureContext.settings.dynamicAssets,
            dynamicAssetsEditDisplay: captureContext.settings.dynamicAssetsEditDisplay,
            parserContext: {
                ...captureContext.parserContext,
                chara: parserChara(),
            },
        } as ProcessScriptCaptureContext
    }

    function parserTranslationContext(): TranslateHTMLContext | undefined {
        const scriptContext = parserScriptContext()
        if (!scriptContext) return undefined
        return {
            scriptContext,
            projectedChatID: captureContext
                ? captureParserIndex
                : parserProjection?.projectedChatID ?? idx,
            chara: parserChara() as unknown as TranslateHTMLContext['chara'],
            cbsConditions: getCbsCondition(),
        }
    }

    function captureMarkdownSettings() {
        if (!captureContext) return undefined
        return {
            customQuotes: captureContext.settings.customQuotes,
            customQuotesData: captureContext.settings.customQuotesData
                ? [...captureContext.settings.customQuotesData] as [string, string, string, string]
                : undefined,
            unformatQuotes: captureContext.settings.unformatQuotes,
            blockquoteStyling: captureContext.settings.blockquoteStyling,
        }
    }

    function finalizeParsedMarkup(value: string) {
        const settings = captureContext?.settings ?? DBState.db
        const scriptContext = parserScriptContext()
        const projectedChatID = captureContext
            ? captureParserIndex
            : parserProjection?.projectedChatID
        const trimmed = scriptContext
            ? trimMarkdown(value, {
                hideAllImages: settings.hideAllImages,
                returnCSSError: settings.returnCSSError,
                parserContext: scriptContext.parserContext,
                chatID: idx,
                projectedChatID,
                cbsConditions: getCbsCondition(),
            })
            : trimMarkdown(value)
        return captureContext
            ? addMetadataToElement(trimmed, modelShortName, captureContext.settings.aiLawApplies ?? false)
            : addMetadataToElement(trimmed, modelShortName)
    }
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

    function trackLiveParseDependencies(
        charArg: string | simpleCharacterArgument,
    ) {
        // Svelte tracks this derived parse job only until its first await. Keep these
        // reads aligned with the synchronous live ParseMarkdown prefix when it changes.
        void translated
        void retranslate
        void firstMessage
        void role
        void name
        void parserProjection
        void captureContext
        void captureParserIndex
        void isEqual(lastCharArg, charArg)
        const settings = DBState.db
        void settings.autoTranslate
        void settings.autoTranslateCachedOnly
        void settings.translatorType
        void settings.translateBeforeHTMLFormatting
        void settings.legacyTranslation
        void settings.showTranslationLoading
        if (charArg || parserProjection) {
            void settings.assetWidth
            void settings.hideAllImages
            void settings.legacyMediaFindings
            void settings.assetMaxDifference
        }
        if (typeof charArg === 'object' && charArg) {
            trackAssetTuples(charArg.additionalAssets)
            trackAssetTuples(charArg.emotionImages)
        }
        if (parserProjection?.context.moduleAssets) {
            trackAssetTuples(parserProjection.context.moduleAssets)
        } else if (charArg) {
            const selectedCharacter = settings.characters?.[selIdState.selId]
            void selectedCharacter?.type
            void selectedCharacter?.chaId
            void selectedCharacter?.additionalAssets
            void selectedCharacter?.emotionImages
            trackAssetTuples(getModuleAssets())
        }
    }

    function trackAssetTuples(
        assets: readonly (readonly unknown[])[] | undefined,
    ) {
        for (const asset of assets ?? []) {
            void asset[0]
            void asset[1]
            void asset[2]
        }
    }

    const markParsing = async (data: string, charArg: string | simpleCharacterArgument, chatID: number, job:ChatBodyParseJob, tries?:number):Promise<string> => {
        const renderCharacter = parserProjection ? parserChara() : charArg
        const translationCharacter = (
            captureContext?.character ?? (parserProjection ? parserChara() : charArg)
        ) as string | simpleCharacterArgument
        if (!captureContext && typeof charArg !== 'string') {
            trackLiveParseDependencies(charArg)
            await yieldToMainThread()
            if (destroyed || job.disposed || activeParseJob !== job)
                return lastParsed
        }
        const parseForRender = (value:string, mode:'normal'|'back'|'pretranslate'|'notrim') => (
            ParseMarkdown(value, renderCharacter, mode, chatID, getCbsCondition(), {
                deferredInlays: job.deferredInlays,
                moduleAssets: captureContext?.moduleAssets ?? parserProjection?.context.moduleAssets,
                assetWidth: captureContext?.settings.assetWidth,
                hideAllImages: captureContext?.settings.hideAllImages,
                legacyMediaFindings: captureContext?.settings.legacyMediaFindings,
                assetMaxDifference: captureContext?.settings.assetMaxDifference,
                characterImageSource: captureContext?.characterImageSource,
                userImageSource: captureContext?.userImageSource,
                scriptContext: parserScriptContext(),
                markdownSettings: captureMarkdownSettings(),
                returnCSSError: captureContext?.settings.returnCSSError,
                projectedChatID: captureContext
                    ? captureParserIndex
                    : parserProjection?.projectedChatID,
            })
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
                    const settings = captureContext?.settings ?? DBState.db
                    if(settings.autoTranslate){
                        if(settings.autoTranslateCachedOnly && settings.translatorType === 'llm'){
                            const cache = settings.translateBeforeHTMLFormatting
                            ? await getLLMCache(data)
                            : !settings.legacyTranslation
                            ? await getLLMCache(await parseForRender(data, 'pretranslate'))
                            : await getLLMCache(await parseForRender(data, mode))
                  
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
                        job.transitional = true
                        return ''
                    }
                } catch (error) {
                    console.error(error)
                }
            }
            if(retranslate || translated){
                const settings = captureContext?.settings ?? DBState.db
                if (settings.showTranslationLoading && !job.preservePendingContent) {
                    lastParsed = `<div style="display:flex;justify-content:center;align-items:center;height:48px;"><div style="animation: spin 1s linear infinite; border-radius: 50%; height: 32px; width: 32px; border: 2px solid #3b82f6; border-top: 2px solid transparent;"></div></div><style>@keyframes spin { to { transform: rotate(360deg); } }</style>`
                }

                let transResult
                
                if(settings.translatorType === 'llm' && settings.translateBeforeHTMLFormatting){
                    await sleep(100)
                    translating = true
                    data = await translateHTML(
                        data,
                        false,
                        translationCharacter,
                        chatID,
                        retranslate,
                        parserTranslationContext(),
                    )
                    translating = false
                    const marked = await parseForRender(data, mode)
                    lastParsedQueue = marked
                    lastCharArg = charArg
                    transResult = marked
                }
                else if(!settings.legacyTranslation){
                    const marked = await parseForRender(data, 'pretranslate')
                    translating = true
                    const translated = await postTranslationParse(await translateHTML(
                        marked,
                        false,
                        translationCharacter,
                        chatID,
                        retranslate,
                        parserTranslationContext(),
                    ), captureMarkdownSettings())
                    translating = false
                    lastParsedQueue = translated
                    lastCharArg = charArg
                    transResult = translated
                }
                else{
                    const marked = await parseForRender(data, mode)
                    translating = true
                    const translated = await translateHTML(
                        marked,
                        false,
                        translationCharacter,
                        chatID,
                        retranslate,
                        parserTranslationContext(),
                    )
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
                if(captureContext) throw error
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
        const settings = captureContext?.settings ?? DBState.db
        if(!settings.newImageHandlingBeta || !bodyRoot){
            return
        }
        const imgs = bodyRoot.querySelectorAll('img:not([data-risu-inlay-token]):not([data-risu-managed-media]):not([src^="data:"]):not([src^="http:"]):not([src^="https:"]):not([src^="blob:"]):not([src^="file:"]):not([src^="tauri:"]):not([noimage])') as NodeListOf<HTMLImageElement>
        
        if (imgs.length > 0) {
            const currentCharacter = captureContext ? null : getCurrentCharacter()
            const styl = captureContext?.assetStyle ?? currentCharacter?.prebuiltAssetStyle ?? ''
            const assets = captureContext
                ? [...captureContext.moduleAssets, ...(captureContext.character?.additionalAssets ?? [])]
                : getModuleAssets().concat(currentCharacter?.additionalAssets ?? [])
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

    function startParsing(requestedRevision: number):ChatBodyParseJob {
        const job:ChatBodyParseJob = {
            promise: Promise.resolve(''),
            deferredInlays: new DeferredInlayMarkerRegistry(),
            disposed: false,
            generation: ++parseGeneration,
            requestedRevision,
            preservePendingContent: lastRenderedRevision !== null && requestedRevision !== lastRenderedRevision,
            settledNotified: false,
            errorNotified: false,
            transitional: false,
            releaseObjectUrls: () => {},
        }
        job.promise = markParsing(msgDisplay, character, idx, job)
        return job
    }

    function disposeParseJob(job:ChatBodyParseJob|null) {
        if (!job || job.disposed) return
        job.disposed = true
        job.releaseObjectUrls()
        job.releaseObjectUrls = () => {}
        job.deferredInlays.clear()
    }

    let markParsingResult = $derived.by(() => shouldRenderRawStreaming ? null : startParsing(renderRevision))

    async function syncObjectUrls(job: ChatBodyParseJob) {
        try {
            await job.promise
            if (job.transitional) return
            if (destroyed || job.disposed || job !== markParsingResult) {
                disposeParseJob(job)
                return
            }
            await tick()
            if (destroyed || job.disposed || job !== markParsingResult) {
                disposeParseJob(job)
                return
            }
            lastRenderedRevision = job.requestedRevision
            let releaseObjectUrls = () => {}
            if (renderRoot) {
                releaseObjectUrls = onCaptureSettled
                    ? await resolveDeferredInlaySources(renderRoot, job.deferredInlays, { rejectOnError: true })
                    : mountDeferredInlaySources(renderRoot, job.deferredInlays)
            }
            else disposeParseJob(job)
            if (destroyed || job.disposed || job !== activeParseJob) {
                releaseObjectUrls()
                return
            }
            job.releaseObjectUrls = releaseObjectUrls
            await checkImg(job)
            await tick()
            if (destroyed || job.disposed || job !== activeParseJob || job.settledNotified) return
            job.settledNotified = true
            onCaptureSettled?.(job.generation)
        }
        catch (error) {
            if (destroyed || job.disposed || job !== activeParseJob || job.errorNotified) return
            job.errorNotified = true
            onCaptureError?.(job.generation, error)
        }
    }

    onDestroy(() => {
        destroyed = true
        disposeParseJob(activeParseJob)
    })

    $effect(() => {
        const result = markParsingResult
        if (activeParseJob !== result) {
            disposeParseJob(activeParseJob)
            activeParseJob = result
        }
        if(shouldRenderRawStreaming){
            disposeParseJob(activeParseJob)
            activeParseJob = null
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
            {@html finalizeParsedMarkup(lastParsed)}
        {:then parsed}
            {@html finalizeParsedMarkup(parsed ?? '')}
        {:catch}
            {@html finalizeParsedMarkup(lastParsed)}
        {/await}
    </span>
{/if}
