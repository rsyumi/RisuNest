<script lang="ts">
    import type { Message } from 'src/ts/storage/database.svelte'
    import type { ChatScreenshotJob } from 'src/ts/chatScreenshotRange'
    import type { simpleCharacterArgument } from 'src/ts/parser/parser.svelte'
    import { onDestroy, tick } from 'svelte'
    import Chat from './Chat.svelte'
    import { CHAT_SCREENSHOT_WIDTH } from 'src/ts/chatScreenshotCapture'

    interface Props {
        totalTurns: number
        character?: simpleCharacterArgument | string | null
        characterName: string
        characterImage?: string
        characterLargePortrait?: boolean
        currentUsername: string
        userImage?: string
        userLargePortrait?: boolean
    }

    let {
        totalTurns,
        character = null,
        characterName,
        characterImage = '',
        characterLargePortrait = false,
        currentUsername,
        userImage = '',
        userLargePortrait = false,
    }: Props = $props()

    type BatchItem = {
        key: string
        turn: number
        message: Message
    }

    type Readiness = {
        generation: number
        remaining: Set<string>
        resolve: (root: HTMLElement) => void
        reject: (error: unknown) => void
        signal: AbortSignal
        abort: () => void
    }

    let viewport: HTMLDivElement
    let batch = $state<BatchItem[]>([])
    let generation = 0
    let readiness: Readiness | null = null
    let destroyed = false

    function cancelReadiness(message: string) {
        const current = readiness
        if (!current) return
        readiness = null
        current.signal.removeEventListener('abort', current.abort)
        current.reject(new DOMException(message, 'AbortError'))
    }

    function reportSettled(batchGeneration: number, key: string) {
        const current = readiness
        if (!current || current.generation !== batchGeneration) return
        current.remaining.delete(key)
        if (current.remaining.size !== 0) return
        readiness = null
        current.signal.removeEventListener('abort', current.abort)
        current.resolve(viewport)
    }

    export async function mountBatch(
        messages: ChatScreenshotJob['messages'],
        firstTurn: number,
        signal: AbortSignal,
    ): Promise<HTMLElement> {
        await unmountBatch()
        if (destroyed || signal.aborted) {
            throw new DOMException('Screenshot capture was cancelled', 'AbortError')
        }

        const batchGeneration = ++generation
        batch = messages.map((message, index) => ({
            key: `${batchGeneration}:${index}`,
            turn: firstTurn + index,
            message: message as Message,
        }))

        const result = new Promise<HTMLElement>((resolve, reject) => {
            const abort = () => cancelReadiness('Screenshot capture was cancelled')
            readiness = {
                generation: batchGeneration,
                remaining: new Set(batch.map((item) => item.key)),
                resolve,
                reject,
                signal,
                abort,
            }
            signal.addEventListener('abort', abort, { once: true })
        })

        await tick()
        for (const item of batch) {
            if (item.message.isComment) reportSettled(batchGeneration, item.key)
        }
        if (batch.length === 0) {
            const current = readiness
            if (current?.generation === batchGeneration) {
                readiness = null
                current.signal.removeEventListener('abort', current.abort)
                current.resolve(viewport)
            }
        }
        return result
    }

    export async function unmountBatch() {
        cancelReadiness('Screenshot capture surface was unmounted')
        batch = []
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
        {#each batch as item (item.key)}
            <Chat
                message={item.message.data}
                isLastMemory={false}
                idx={item.turn - 1}
                totalLength={totalTurns}
                img={item.message.role === 'user' ? userImage : characterImage}
                character={character}
                largePortrait={item.message.role === 'user' ? userLargePortrait : characterLargePortrait}
                messageGenerationInfo={item.message.generationInfo}
                role={item.message.role}
                name={item.message.role === 'user' ? currentUsername : characterName}
                isComment={item.message.isComment ?? false}
                disabled={item.message.disabled ?? false}
                onCaptureSettled={() => reportSettled(generation, item.key)}
            />
        {/each}
    </div>
</div>
