<script lang="ts">
    import type { Message } from 'src/ts/storage/database.svelte'
    import type { ChatScreenshotJob, FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import { getFileSrc } from 'src/ts/globalApi.svelte'
    import { onDestroy, tick } from 'svelte'
    import Chat from './Chat.svelte'
    import { CHAT_SCREENSHOT_PARSE_TIMEOUT_MS, CHAT_SCREENSHOT_WIDTH } from 'src/ts/chatScreenshotCapture'

    type BatchItem = {
        key: string
        generation: number
        turn: number
        parserIndex: number
        message: Message
    }

    type Readiness = {
        generation: number
        remaining: Set<string>
        resolve: (root: HTMLElement) => void
        reject: (error: unknown) => void
        signal: AbortSignal
        abort: () => void
        timeout: ReturnType<typeof setTimeout>
    }

    let viewport: HTMLDivElement
    let batch = $state<BatchItem[]>([])
    let renderContext = $state<FrozenChatScreenshotRenderContext | null>(null)
    let characterImage = $state('')
    let userImage = $state('')
    let generation = 0
    let readiness: Readiness | null = null
    let destroyed = false

    async function resolvePortrait(source: string, hideAllImages: boolean) {
        if (hideAllImages || !source) return ''
        return `background: url("${await getFileSrc(source)}");background-size: cover;`
    }

    function cancelReadiness(message: string) {
        const current = readiness
        if (!current) return
        readiness = null
        clearTimeout(current.timeout)
        current.signal.removeEventListener('abort', current.abort)
        current.reject(new DOMException(message, 'AbortError'))
    }

    function reportError(batchGeneration: number, error: unknown) {
        const current = readiness
        if (!current || current.generation !== batchGeneration) return
        readiness = null
        clearTimeout(current.timeout)
        current.signal.removeEventListener('abort', current.abort)
        current.reject(error)
    }

    function reportSettled(batchGeneration: number, key: string) {
        const current = readiness
        if (!current || current.generation !== batchGeneration) return
        current.remaining.delete(key)
        if (current.remaining.size !== 0) return
        readiness = null
        clearTimeout(current.timeout)
        current.signal.removeEventListener('abort', current.abort)
        current.resolve(viewport)
    }

    export async function mountBatch(
        messages: ChatScreenshotJob['messages'],
        firstTurn: number,
        context: FrozenChatScreenshotRenderContext,
        signal: AbortSignal,
    ): Promise<HTMLElement> {
        await unmountBatch()
        if (destroyed || signal.aborted) {
            throw new DOMException('Screenshot capture was cancelled', 'AbortError')
        }

        const batchGeneration = ++generation
        const nextBatch = messages.map((message, index) => ({
            key: `${batchGeneration}:${index}`,
            generation: batchGeneration,
            turn: firstTurn + index,
            parserIndex: firstTurn + index - 1 - (context.historyStartIndex ?? 0),
            message: message as Message,
        }))

        const result = new Promise<HTMLElement>((resolve, reject) => {
            const abort = () => cancelReadiness('Screenshot capture was cancelled')
            readiness = {
                generation: batchGeneration,
                remaining: new Set(nextBatch.map((item) => item.key)),
                resolve,
                reject,
                signal,
                abort,
                timeout: setTimeout(
                    () => reportError(
                        batchGeneration,
                        new Error('Screenshot capture readiness timed out'),
                    ),
                    CHAT_SCREENSHOT_PARSE_TIMEOUT_MS,
                ),
            }
            signal.addEventListener('abort', abort, { once: true })
        })

        try {
            ;[characterImage, userImage] = await Promise.race([
                Promise.all([
                    resolvePortrait(context.characterImageSource, context.settings.hideAllImages),
                    resolvePortrait(context.userImageSource, context.settings.hideAllImages),
                ]),
                result.then(() => {
                    throw new DOMException('Screenshot capture setup was cancelled', 'AbortError')
                }),
            ])
        } catch (error) {
            reportError(batchGeneration, error)
            return result
        }

        if (destroyed || signal.aborted) {
            cancelReadiness('Screenshot capture was cancelled')
            return result
        }
        renderContext = context
        batch = nextBatch
        await tick()
        if (batch.length === 0) {
            const current = readiness
            if (current?.generation === batchGeneration) {
                readiness = null
                clearTimeout(current.timeout)
                current.signal.removeEventListener('abort', current.abort)
                current.resolve(viewport)
            }
        }
        return result
    }

    export async function unmountBatch() {
        cancelReadiness('Screenshot capture surface was unmounted')
        batch = []
        renderContext = null
        characterImage = ''
        userImage = ''
        await tick()
    }

    export async function dispose() {
        await unmountBatch()
    }

    onDestroy(() => {
        destroyed = true
        cancelReadiness('Screenshot capture surface was destroyed')
    })
</script>

<div
    class="chat-screenshot-capture-surface overflow-hidden bg-bgcolor text-textcolor"
    data-screenshot-viewport
    bind:this={viewport}
    style:width={`${CHAT_SCREENSHOT_WIDTH}px`}
    style:position="fixed"
    style:left="-100000px"
    style:top="0"
    style:pointer-events="none"
    aria-hidden="true"
>
    <div data-screenshot-content>
        {#if renderContext}
            {#each batch as item (item.key)}
                <Chat
                    message={item.message.data}
                    name={item.message.role === 'user'
                        ? renderContext.userName
                        : renderContext.characterName}
                    largePortrait={item.message.role === 'user'
                        ? renderContext.userLargePortrait
                        : renderContext.characterLargePortrait}
                    isLastMemory={false}
                    img={item.message.role === 'user' ? userImage : characterImage}
                    idx={item.turn - 1}
                    messageGenerationInfo={item.message.generationInfo ?? null}
                    role={item.message.role}
                    totalLength={renderContext.totalTurns ?? item.turn}
                    character={renderContext.character as import('src/ts/parser/parser.svelte').simpleCharacterArgument | null}
                    firstMessage={false}
                    isComment={item.message.isComment ?? false}
                    disabled={item.message.disabled ?? false}
                    captureContext={renderContext}
                    captureMessage={item.message}
                    captureParserIndex={item.parserIndex}
                    onCaptureSettled={() => reportSettled(item.generation, item.key)}
                    onCaptureError={(_generation, error) => reportError(item.generation, error)}
                />
            {/each}
        {/if}
    </div>
</div>
