<script lang="ts">
    import { getFileImageSource } from 'src/ts/globalApi.svelte'
    import isEqual from "lodash/isEqual"
    import { DBState, selIdState } from 'src/ts/stores.svelte'
    import { sleep } from "src/ts/util"
    import { alertError } from "../../ts/alert"
    import { addMetadataToElement, getDistance, ParseMarkdown, postTranslationParse, trimMarkdown, type CbsConditions, type simpleCharacterArgument } from "../../ts/parser/parser.svelte"
    import { getLLMCache, isExpTranslator, translateHTML, type TranslateHTMLContext } from "../../ts/translator/translator"
    import { doingChat } from 'src/ts/process/generationState'
    import { get } from 'svelte/store'
    import { getModuleAssets } from "src/ts/process/modules";
    import { getCurrentCharacter } from "src/ts/storage/database.svelte";
    import { getFileSrc } from "src/ts/globalApi.svelte";
    import { DeferredInlayMarkerRegistry, mountDeferredInlaySources, resolveDeferredInlaySources } from "src/ts/process/files/inlayRenderSource";
    // settled() resolves once the DOM update commits; tick() would also wait for a frame or timer.
    import { onDestroy, settled as domSettled, untrack } from 'svelte'
    import type { FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { ProcessScriptCaptureContext } from 'src/ts/process/scripts'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type { character as CharacterRecord } from 'src/ts/storage/database.svelte'
    import { yieldToMainThread } from 'src/ts/ui/yieldToUi'
    import StreamingThoughtPreviewView from './StreamingThoughtPreview.svelte'
    import type { StreamingThoughtPreview } from '../../ts/parser/streamingThoughtPreview'
    import type { StreamingThoughtMode } from '../../ts/storage/database.svelte'
    import { mountBoundedThoughtExpansion, type BoundedThoughtExpansion } from './boundedThoughtExpansion'
    import { DisplayReadRecord, recordChatVariableReads, type DisplayRowReads } from 'src/ts/process/scriptVariableReads'
    import { nextDisplayListenerOrder } from 'src/ts/process/luaDisplayTimeouts'

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
        reloadRevision?: string
        bodyRoot?: HTMLElement|null
        modelShortName: string
        renderRawStreaming?: boolean
        rawStreamingText?: string
        thoughtPreview?: StreamingThoughtPreview | null
        streamingThoughtMode?: StreamingThoughtMode
        deferStreamingDisplay?: boolean
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
        captureContext?: FrozenChatScreenshotRenderContext
        captureParserIndex?: number
        parserProjection?: BoundedLiveChatParserProjection
        parserAbortSignal?: AbortSignal
        onDisplaySettled?: () => void
        displayRow?: DisplayRowReads
        /** When the view asked for this row, from `nextDisplayListenerOrder`. */
        displayRequestOrder?: number
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
        reloadRevision = '',
        bodyRoot,
        modelShortName = '',
        renderRawStreaming = false,
        rawStreamingText = '',
        thoughtPreview = null,
        streamingThoughtMode = 'off',
        deferStreamingDisplay = false,
        onCaptureSettled,
        onCaptureError,
        captureContext,
        captureParserIndex = idx,
        parserProjection,
        parserAbortSignal,
        onDisplaySettled,
        displayRow,
        displayRequestOrder,
    }: Props =  $props()

    // svelte-ignore non_reactive_update
    let lastParsed = ''
    let lastCharArg:string|simpleCharacterArgument = null
    let lastChatId = -10
    let thoughtOpenStates: boolean[] = []
    let previewThoughtOpen: boolean | undefined

    let destroyed = false

    interface ChatBodyParseJob {
        controller: AbortController
        removeExternalAbortListener(): void
        promise: Promise<string>
        finalizeMarkup(value: string, deferredInlays?: DeferredInlayMarkerRegistry): string
        deferredInlays: DeferredInlayMarkerRegistry
        disposed: boolean
        generation: number
        requestedRevision: number
        preservePendingContent: boolean
        settledNotified: boolean
        errorNotified: boolean
        transitional: boolean
        onDisplaySettled?: () => void
        releaseObjectUrls: () => void
        thoughtExpansion: BoundedThoughtExpansion | null
        reads?: DisplayReadRecord
        rebuildRevision: number
        translatedDuringGeneration: boolean
        displayRequestOrder: number
    }

    let activeParseJob: ChatBodyParseJob|null = null
    let displayedParseJob: ChatBodyParseJob|null = null
    let displayedHtml = $state('')
    let displayEpoch = $state(0)
    type PendingPreview = {
        thought: StreamingThoughtPreview | null
        source: string
        mode: StreamingThoughtMode
    }
    let pendingPreview = $state<PendingPreview | null>(null)
    let currentThoughtPreview = $derived(captureContext ? null : thoughtPreview)

    let parseGeneration = 0
    let lastRenderedRevision: number | null = null
    // The translator leaves a live row untranslated while a reply is generated,
    // so a row translated then parses again once the generation ends.
    let generationEndRevision = $state(0)
    let generating = false
    const unsubscribeGeneration = doingChat.subscribe((busy) => {
        if (generating && !busy && activeParseJob?.translatedDuringGeneration) generationEndRevision += 1
        generating = busy
    })

    function noteGenerationTranslation(job: ChatBodyParseJob) {
        if (!captureContext && get(doingChat) && isExpTranslator(DBState.db)) job.translatedDuringGeneration = true
    }

    function parserChara(): string | CharacterRecord {
        const parserCharacter = captureContext?.parserContext.character
            ?? parserProjection?.context.parserContext.character
        const character = parserCharacter as CharacterRecord
        return character
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
            ...(captureContext ? {} : { liveProjection: true as const }),
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

    function createMarkupFinalizer(reads?: DisplayReadRecord) {
        const settings = captureContext?.settings ?? DBState.db
        const scriptContext = parserScriptContext()
        const projectedChatID = captureContext
            ? captureParserIndex
            : parserProjection?.projectedChatID
        const renderContext = {
            hideAllImages: settings.hideAllImages,
            returnCSSError: settings.returnCSSError,
            parserContext: scriptContext?.parserContext,
            chatID: idx,
            projectedChatID,
            cbsConditions: getCbsCondition(),
        }
        const model = modelShortName
        const frozenLawApplies = captureContext
            ? (captureContext.settings.aiLawApplies ?? false)
            : undefined
        // CSS decoding must use this job's context and publish with its HTML.
        // Re-evaluating it in the template restyles the old DOM during a refresh.
        return (value: string, deferredInlays?: DeferredInlayMarkerRegistry) =>
            addMetadataToElement(
                recordChatVariableReads(reads, () => trimMarkdown(value, { ...renderContext, deferredInlays })),
                model,
                frozenLawApplies,
            )
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

    let shouldRenderRawStreaming = $derived(
        !captureContext &&
            renderRawStreaming &&
            (deferStreamingDisplay || (!translated && !retranslate)),
    )
    let displayedPreview = $derived<PendingPreview | null>(
        currentThoughtPreview || shouldRenderRawStreaming
            ? {
                  thought: currentThoughtPreview,
                  source: rawStreamingText,
                  mode: streamingThoughtMode,
              }
            : pendingPreview,
    )
    // Rendered markup stays a direct child of the parent text root, as upstream
    // renders it. While a preview is shown, no rendered markup is mounted there.
    let renderRoot = $derived(displayedPreview ? undefined : (bodyRoot ?? undefined))

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

    function alertParseError(error: any) {
        const wrapped = new Error(`Error while parsing chat message: ${error?.message ?? error}`)
        if(error instanceof Error){
            wrapped.stack = error.stack
        }
        alertError(wrapped)
    }

    const markParsing = async (data: string, charArg: string | simpleCharacterArgument, chatID: number, job:ChatBodyParseJob, tries?:number):Promise<string> => {
        const thoughtMode = captureContext ? 'off' : streamingThoughtMode
        job.controller.signal.throwIfAborted()
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
                streamingThoughtMode: thoughtMode,
                signal: job.controller.signal,
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
                variableReads: job.reads,
                displayRequestOrder: job.displayRequestOrder,
            })
        )
        // track 'translated' and 'retranslate' state
        translated;
        retranslate;
        let lastParsedQueue = ''
        let mode = 'notrim' as const
        let translationAttempted = false
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

                    // State change of `translated` triggers markParsing again,
                    // causing redundant translation attempts
                    if (lastTranslated !== translateText) {
                        job.transitional = true
                        setTimeout(() => {
                            if (destroyed || job.disposed || activeParseJob !== job) return
                            translated = translateText
                        }, 10)
                        return ''
                    }
                } catch (error) {
                    console.error(error)
                }
            }
            if(retranslate || translated){
                translationAttempted = true
                // Translation runs its own script pass, which this record does not follow.
                job.reads?.markUntracked()
                const settings = captureContext?.settings ?? DBState.db
                if (settings.showTranslationLoading && !job.preservePendingContent) {
                    lastParsed = `<div style="display:flex;justify-content:center;align-items:center;height:48px;"><div style="animation: spin 1s linear infinite; border-radius: 50%; height: 32px; width: 32px; border: 2px solid #3b82f6; border-top: 2px solid transparent;"></div></div><style>@keyframes spin { to { transform: rotate(360deg); } }</style>`
                    const pendingMarkup = lastParsed
                    queueMicrotask(() => {
                        if (!destroyed && !job.disposed && activeParseJob === job && !displayedParseJob) {
                            displayedHtml = job.finalizeMarkup(pendingMarkup)
                        }
                    })
                }

                let transResult
                
                if(settings.translatorType === 'llm' && settings.translateBeforeHTMLFormatting){
                    await sleep(100)
                    translating = true
                    noteGenerationTranslation(job)
                    data = await translateHTML(
                        data,
                        false,
                        translationCharacter,
                        chatID,
                        retranslate,
                        parserTranslationContext(),
                        job.controller.signal,
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
                    noteGenerationTranslation(job)
                    const translated = await postTranslationParse(await translateHTML(
                        marked,
                        false,
                        translationCharacter,
                        chatID,
                        retranslate,
                        parserTranslationContext(),
                        job.controller.signal,
                    ), captureMarkdownSettings())
                    translating = false
                    lastParsedQueue = translated
                    lastCharArg = charArg
                    transResult = translated
                }
                else{
                    const marked = await parseForRender(data, mode)
                    translating = true
                    noteGenerationTranslation(job)
                    const translated = await translateHTML(
                        marked,
                        false,
                        translationCharacter,
                        chatID,
                        retranslate,
                        parserTranslationContext(),
                        job.controller.signal,
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
            if (job.controller.signal.aborted) throw error
            if (translationAttempted && !captureContext) {
                translating = false
                // A failed translation leaves the message as it renders untranslated.
                job.deferredInlays.clear()
                job.deferredInlays = new DeferredInlayMarkerRegistry()
                const marked = await parseForRender(data, mode).catch(() => null)
                if (marked !== null) {
                    alertParseError(error)
                    lastParsedQueue = marked
                    lastCharArg = charArg
                    return marked
                }
            }
            //retry
            if(tries > 2){
                if(captureContext) throw error
                alertParseError(error)
                return data
            }
            if(job.disposed) return data
            job.deferredInlays.clear()
            job.deferredInlays = new DeferredInlayMarkerRegistry()
            return await markParsing(data, charArg, chatID, job, (tries ?? 0) + 1)
        }
        finally{
            //since trimMarkdown is fast, we don't need to cache it
            if (!job.disposed && !job.controller.signal.aborted && activeParseJob === job) {
                lastParsed = lastParsedQueue
            }
        }
    }

    const checkImg = async (job: ChatBodyParseJob, root: ParentNode) => {
        const settings = captureContext?.settings ?? DBState.db
        if(!settings.newImageHandlingBeta || settings.hideAllImages){
            return
        }
        const imgs = root.querySelectorAll('img[src]:not([data-risu-inlay-slot]):not([data-risu-inlay-token]):not([data-risu-managed-media]):not([src^="data:"]):not([src^="http:"]):not([src^="https:"]):not([src^="blob:"]):not([src^="file:"]):not([src^="tauri:"]):not([noimage])') as NodeListOf<HTMLImageElement>
        
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

            async function attachImage(img: HTMLImageElement, key: string, originalName: string) {
                try {
                    const image = await getFileImageSource(key)
                    const url = image?.url ?? await getFileSrc(key)
                    const isCurrent = () => !destroyed && !job.disposed && job === activeParseJob && !job.controller.signal.aborted
                    if (!isCurrent() || img.getAttribute('src')?.toLocaleLowerCase() !== originalName) return
                    img.src = url
                    if (image) job.deferredInlays.rememberAssetImage(image)
                } catch (error) {
                    if (captureContext) throw error
                    if (!job.controller.signal.aborted) console.warn('Image source could not be resolved')
                }
            }

            await Promise.all(Array.from(imgs).map(async (img) => {
                const name = img.getAttribute('src')?.toLocaleLowerCase() || ''

                if(
                    name.length > 200 ||
                    name.includes(':')
                ){
                    img.setAttribute('noimage', 'true')
                    return
                }
                
                const foundAsset = exactAssets.get(name)
                if(foundAsset){
                    img.classList.add('root-loaded-image')
                    img.classList.add('root-loaded-image-' + styl)
                    await attachImage(img, foundAsset, name)
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
                    await attachImage(img, currentFound, name)
                    if (destroyed || job.disposed || job !== activeParseJob) return

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
            job.deferredInlays.applyAssetImageHints(root)
        }
    }

    // The parse the view asked for keeps the view's request order. A later parse is a new request.
    let consumedDisplayRequestOrder: number | undefined
    function takeDisplayRequestOrder(): number {
        const requested = untrack(() => displayRequestOrder)
        if (requested === undefined || requested === consumedDisplayRequestOrder) return nextDisplayListenerOrder()
        consumedDisplayRequestOrder = requested
        return requested
    }

    function startParsing(requestedRevision: number):ChatBodyParseJob {
        const controller = new AbortController()
        const externalSignal = parserAbortSignal
        const abort = () => controller.abort(externalSignal?.reason)
        if (externalSignal?.aborted) abort()
        else externalSignal?.addEventListener('abort', abort, { once: true })
        const reads = displayRow ? new DisplayReadRecord() : undefined
        // The pending parse counts until it is shown, so a change it already read refreshes it.
        if (reads) displayRow.track('body-pending', reads)
        const job:ChatBodyParseJob = {
            controller,
            removeExternalAbortListener: () => externalSignal?.removeEventListener('abort', abort),
            promise: Promise.resolve(''),
            finalizeMarkup: createMarkupFinalizer(reads),
            deferredInlays: new DeferredInlayMarkerRegistry(),
            disposed: false,
            generation: ++parseGeneration,
            requestedRevision,
            preservePendingContent: lastRenderedRevision !== null,
            settledNotified: false,
            errorNotified: false,
            transitional: false,
            onDisplaySettled,
            releaseObjectUrls: () => {},
            thoughtExpansion: null,
            reads,
            rebuildRevision: displayRow?.rebuildRevision ?? 0,
            translatedDuringGeneration: false,
            displayRequestOrder: takeDisplayRequestOrder(),
        }
        job.promise = markParsing(msgDisplay, character, idx, job)
        return job
    }

    function disposeParseJob(job:ChatBodyParseJob|null) {
        if (!job || job.disposed) return
        job.disposed = true
        job.controller.abort()
        job.removeExternalAbortListener()
        job.releaseObjectUrls()
        job.releaseObjectUrls = () => {}
        job.thoughtExpansion?.dispose()
        job.thoughtExpansion = null
        job.deferredInlays.clear()
    }

    let markParsingResult = $derived.by(() => {
        void reloadRevision
        void generationEndRevision
        return currentThoughtPreview || shouldRenderRawStreaming
            ? null
            : startParsing(renderRevision)
    })

    async function syncObjectUrls(job: ChatBodyParseJob) {
        try {
            const parsed = await job.promise
            if (job.transitional) return
            if (
                destroyed ||
                job.disposed ||
                job.controller.signal.aborted ||
                job !== markParsingResult
            ) {
                if (job !== displayedParseJob) disposeParseJob(job)
                return
            }
            // Keep the current DOM and its media leases until the replacement is ready.
            let html = job.finalizeMarkup(parsed, job.deferredInlays)
            if ((captureContext?.settings ?? DBState.db).newImageHandlingBeta && /<img\b/i.test(html)) {
                const template = document.createElement('template')
                template.innerHTML = html
                await checkImg(job, template.content)
                if (destroyed || job.disposed || job.controller.signal.aborted || job !== markParsingResult) {
                    if (job !== displayedParseJob) disposeParseJob(job)
                    return
                }
                html = template.innerHTML
            }
            const previousDisplay = displayedParseJob
            const openThoughts = Array.from(
                renderRoot?.querySelectorAll<HTMLDetailsElement>(
                    'details[data-risu-thought]',
                ) ?? [],
            ).map((element) => element.open)
            if (openThoughts.length) thoughtOpenStates = openThoughts

            // Identical markup keeps its DOM unless a script reloaded the row.
            const retainMarkup =
                html === displayedHtml && previousDisplay?.settledNotified &&
                job.rebuildRevision === previousDisplay.rebuildRevision
            if (retainMarkup) {
                job.releaseObjectUrls = previousDisplay.releaseObjectUrls
                previousDisplay.releaseObjectUrls = () => {}
                job.thoughtExpansion = previousDisplay.thoughtExpansion
                previousDisplay.thoughtExpansion = null
                job.deferredInlays.clear()
            } else if (html === displayedHtml) {
                displayEpoch += 1
            }
            displayedParseJob = job
            displayedHtml = html
            if (job.reads) displayRow?.track('body', job.reads)
            pendingPreview = null
            await domSettled()
            if (previousDisplay !== job) disposeParseJob(previousDisplay)
            if (destroyed || job.disposed || job !== markParsingResult) {
                if (job !== displayedParseJob) disposeParseJob(job)
                return
            }
            lastRenderedRevision = job.requestedRevision
            if (!captureContext && renderRoot && !job.thoughtExpansion) {
                job.thoughtExpansion = mountBoundedThoughtExpansion(renderRoot)
            }
            const settledThoughts = renderRoot?.querySelectorAll<HTMLDetailsElement>(
                'details[data-risu-thought]',
            )
            settledThoughts?.forEach((element, index) => {
                const fromPreview = previewThoughtOpen !== undefined && index === settledThoughts.length - 1
                const open = fromPreview ? previewThoughtOpen : thoughtOpenStates[index]
                // A large thought expanded only in the preview settles collapsed.
                if (open !== undefined && !(fromPreview && job.thoughtExpansion?.managed.has(element))) element.open = open
            })
            thoughtOpenStates = Array.from(settledThoughts ?? [], (element) => element.open)
            previewThoughtOpen = undefined
            let releaseObjectUrls = () => {}
            if (retainMarkup) {
                releaseObjectUrls = job.releaseObjectUrls
            } else if (renderRoot) {
                releaseObjectUrls = onCaptureSettled
                    ? await resolveDeferredInlaySources(
                          renderRoot,
                          job.deferredInlays,
                          { rejectOnError: true },
                      )
                    : mountDeferredInlaySources(renderRoot, job.deferredInlays)
            } else disposeParseJob(job)
            if (destroyed || job.disposed || job !== activeParseJob) {
                releaseObjectUrls()
                return
            }
            job.releaseObjectUrls = releaseObjectUrls
            await domSettled()
            if (
                destroyed ||
                job.disposed ||
                job !== activeParseJob ||
                job.settledNotified
            )
                return
            job.settledNotified = true
            job.onDisplaySettled?.()
            onCaptureSettled?.(job.generation)
        } catch (error) {
            if (
                destroyed ||
                job.disposed ||
                job !== activeParseJob ||
                job.errorNotified
            )
                return
            job.errorNotified = true
            job.onDisplaySettled?.()
            onCaptureError?.(job.generation, error)
        }
    }

    onDestroy(() => {
        destroyed = true
        unsubscribeGeneration()
        disposeParseJob(activeParseJob)
        disposeParseJob(displayedParseJob)
    })

    $effect(() => {
        const result = markParsingResult
        if (activeParseJob !== result) {
            if (activeParseJob === displayedParseJob) {
                activeParseJob?.controller.abort()
                activeParseJob?.removeExternalAbortListener()
            } else disposeParseJob(activeParseJob)
            activeParseJob = result
        }
        if(currentThoughtPreview || shouldRenderRawStreaming){
            pendingPreview = {
                thought: currentThoughtPreview,
                source: rawStreamingText,
                mode: streamingThoughtMode,
            }

            disposeParseJob(activeParseJob)
            activeParseJob = null
            disposeParseJob(displayedParseJob)
            displayedParseJob = null
            displayedHtml = ''
            const settled = onDisplaySettled
            void domSettled().then(() => {
                if (!destroyed && (currentThoughtPreview || shouldRenderRawStreaming)) settled?.()
            })
            return
        }
        if (!result) return
        void syncObjectUrls(result)
    })
</script>

{#if displayedPreview}
    {#if displayedPreview.thought}
        <StreamingThoughtPreviewView preview={displayedPreview.thought} mode={displayedPreview.mode} source={displayedPreview.source} onExpandedChange={(open) => { previewThoughtOpen = open }} />
    {:else}
        <span class="whitespace-pre-wrap">{displayedPreview.source}</span>
    {/if}
{:else}
    {#key displayEpoch}
        {@html displayedHtml}
    {/key}
{/if}
