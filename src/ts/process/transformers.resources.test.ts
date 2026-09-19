import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import type { TextGenerationConfig } from '@huggingface/transformers'

const generationConfig = { max_new_tokens: 1 } as TextGenerationConfig

const harness = vi.hoisted(() => ({
    pipeline: vi.fn(),
    loadAsset: vi.fn(),
    env: {} as Record<string, any>,
}))

vi.mock('@huggingface/transformers', () => ({ pipeline: harness.pipeline, env: harness.env }))
vi.mock('src/ts/globalApi.svelte', () => ({ loadAsset: harness.loadAsset, saveAsset: vi.fn() }))
vi.mock('src/ts/util', () => ({
    selectSingleFile: vi.fn(),
    asBuffer: (data: Uint8Array) => data,
}))

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => { resolve = done })
    return { promise, resolve }
}

function model(result: unknown) {
    return Object.assign(vi.fn().mockResolvedValue(result), {
        dispose: vi.fn().mockResolvedValue(undefined),
    })
}

class AudioContextFixture {
    static instances: AudioContextFixture[] = []
    destination = {}
    close = vi.fn().mockResolvedValue(undefined)
    decodeAudioData = vi.fn().mockResolvedValue({})
    source = {
        buffer: null as unknown,
        onended: null as (() => void) | null,
        connect: vi.fn(),
        disconnect: vi.fn(),
        start: vi.fn(),
    }
    createBufferSource = vi.fn(() => this.source)
    constructor() { AudioContextFixture.instances.push(this) }
}

beforeEach(() => {
    vi.resetModules()
    harness.pipeline.mockReset()
    harness.loadAsset.mockReset()
    for (const key of Object.keys(harness.env)) delete harness.env[key]
    AudioContextFixture.instances = []
    vi.stubGlobal('AudioContext', AudioContextFixture)
    vi.stubGlobal('location', { origin: 'https://synthetic.invalid' })
    vi.stubGlobal('caches', {
        open: vi.fn().mockResolvedValue({ put: vi.fn(), match: vi.fn() }),
    })
})
afterEach(() => {
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
})

it.each(['text', 'summary', 'image'] as const)('releases a one-shot %s model on success and inference failure', async (kind) => {
    const result = kind === 'summary' ? [{ summary_text: 'summary' }] : [{ generated_text: 'answer' }]
    const instance = model(result)
    harness.pipeline.mockResolvedValue(instance)
    const api = await import('./transformers')
    const run = () => kind === 'text' ? api.runTransformers('text', 'synthetic/model', generationConfig)
        : kind === 'summary' ? api.runSummarizer('text') : api.runImageEmbedding('data:image/png;base64,AA==')
    await run()
    expect(instance.dispose).toHaveBeenCalledTimes(1)
    instance.mockRejectedValueOnce(new Error('inference failure'))
    instance.dispose.mockRejectedValueOnce(new Error('cleanup failure'))
    await expect(run()).rejects.toThrow('inference failure')
    expect(instance.dispose).toHaveBeenCalledTimes(2)
})

it('does not replace a successful result when cleanup fails', async () => {
    const instance = model([{ generated_text: 'answer' }])
    instance.dispose.mockRejectedValue(new Error('cleanup failure'))
    harness.pipeline.mockResolvedValue(instance)
    const { runTransformers } = await import('./transformers')
    await expect(runTransformers('text', 'synthetic/model', generationConfig)).resolves.toEqual({ generated_text: 'answer' })
})

it('waits for active embedding inference before replacing the model', async () => {
    const pending = deferred<{ data: Float32Array }>()
    const first = model(null)
    first.mockReturnValueOnce(pending.promise)
    const second = model({ data: new Float32Array([7, 8]) })
    harness.pipeline.mockResolvedValueOnce(first).mockResolvedValueOnce(second)
    const { runEmbedding } = await import('./transformers')
    const before = runEmbedding(['a'], 'Xenova/all-MiniLM-L6-v2', 'wasm')
    await vi.waitFor(() => expect(first).toHaveBeenCalledTimes(1))
    const after = runEmbedding(['b'], 'nomic-ai/nomic-embed-text-v1.5', 'wasm')
    await Promise.resolve()
    expect(first.dispose).not.toHaveBeenCalled()
    expect(harness.pipeline).toHaveBeenCalledTimes(1)
    pending.resolve({ data: new Float32Array([1, 2]) })
    expect(await before).toEqual([new Float32Array([1, 2])])
    expect(await after).toEqual([new Float32Array([7, 8])])
    expect(first.dispose).toHaveBeenCalledTimes(1)
    expect(second.dispose).not.toHaveBeenCalled()
})

it('retries a failed replacement without retaining a disposed embedding model', async () => {
    const first = model({ data: new Float32Array([1]) })
    const second = model({ data: new Float32Array([2]) })
    harness.pipeline.mockResolvedValueOnce(first)
        .mockRejectedValueOnce(new Error('load failure')).mockResolvedValueOnce(second)
    const { runEmbedding } = await import('./transformers')
    await runEmbedding(['a'], 'Xenova/all-MiniLM-L6-v2', 'wasm')
    await expect(runEmbedding(['b'], 'nomic-ai/nomic-embed-text-v1.5', 'wasm')).rejects.toThrow('load failure')
    expect(await runEmbedding(['c'], 'nomic-ai/nomic-embed-text-v1.5', 'wasm')).toEqual([new Float32Array([2])])
    expect(first.dispose).toHaveBeenCalledTimes(1)
    expect(first).toHaveBeenCalledTimes(1)
})

it('keeps embedding order and values across low-spec batches without replacing the model', async () => {
    const { setRuntimePerformanceProfile } = await import('../runtimePerformanceProfile')
    setRuntimePerformanceProfile('low-spec')
    try {
        const instance = model(null)
        instance.mockImplementation(async (batch: string[]) => ({ data: Float32Array.from(batch.flatMap((text) => [Number(text), -Number(text)])) }))
        harness.pipeline.mockResolvedValue(instance)
        const { runEmbedding } = await import('./transformers')
        const input = Array.from({ length: 19 }, (_, i) => String(i))
        const output = await runEmbedding(input, undefined, 'wasm')
        expect(instance.mock.calls.map(([batch]) => batch.length)).toEqual([8, 8, 3])
        expect(output).toEqual(input.map((text) => new Float32Array([Number(text), -Number(text)])))
        expect(harness.pipeline).toHaveBeenCalledTimes(1)
        expect(instance.dispose).not.toHaveBeenCalled()
    } finally {
        setRuntimePerformanceProfile('normal')
    }
})

it('does not initialize models for empty embeddings, absent speech, or idle cleanup', async () => {
    const { runEmbedding, runVITS, releaseIdleTransformerModels } = await import('./transformers')
    await releaseIdleTransformerModels()
    expect(await runEmbedding([], undefined, 'wasm')).toEqual([])
    await runVITS('text', null)
    expect(harness.pipeline).not.toHaveBeenCalled()
    expect(caches.open).not.toHaveBeenCalled()
})

it('clears replaced custom-model asset mappings and releases completed speech audio', async () => {
    const output = { sampling_rate: 16000, audio: new Float32Array([0, 0.1, 0]) }
    const first = model(output)
    const second = model(output)
    harness.pipeline.mockResolvedValueOnce(first).mockResolvedValueOnce(second)
    harness.loadAsset.mockResolvedValue(new Uint8Array([1, 2]))
    const { runVITS } = await import('./transformers')
    await runVITS('first', { id: 'local-a', files: { 'model.onnx': 'synthetic-asset' } })
    const url = `${harness.env.localModelPath}local-a/model.onnx`
    expect(await harness.env.customCache.match(url)).toBeInstanceOf(Response)
    await runVITS('second', 'synthetic/model-b')
    expect(first.dispose).toHaveBeenCalledTimes(1)
    expect(await harness.env.customCache.match(url)).toBeUndefined()
    expect(harness.loadAsset).toHaveBeenCalledTimes(1)
    for (const audio of AudioContextFixture.instances) {
        expect(audio.close).not.toHaveBeenCalled()
        const ended = audio.source.onended!
        ended()
        ended()
        expect(audio.source.disconnect).toHaveBeenCalledTimes(1)
        expect(audio.source.buffer).toBeNull()
        expect(audio.close).toHaveBeenCalledTimes(1)
    }
})

it.each(['decode', 'start'] as const)('releases audio after %s failure and preserves the error', async (failure) => {
    const output = { sampling_rate: 16000, audio: new Float32Array([0]) }
    harness.pipeline.mockResolvedValue(model(output))
    class BrokenAudio extends AudioContextFixture {
        constructor() {
            super()
            if (failure === 'decode') this.decodeAudioData.mockRejectedValueOnce(new Error('audio failure'))
            else this.source.start.mockImplementationOnce(() => { throw new Error('audio failure') })
        }
    }
    vi.stubGlobal('AudioContext', BrokenAudio)
    const { runVITS } = await import('./transformers')
    await expect(runVITS('text')).rejects.toThrow('audio failure')
    expect(AudioContextFixture.instances[0].close).toHaveBeenCalledTimes(1)
})

it('releases idle models once, reloads on demand, and leaves active audio playing', async () => {
    const embedding = model({ data: new Float32Array([1]) })
    const speech = model({ sampling_rate: 16000, audio: new Float32Array([0]) })
    const reloaded = model({ data: new Float32Array([2]) })
    harness.pipeline.mockResolvedValueOnce(embedding).mockResolvedValueOnce(speech).mockResolvedValueOnce(reloaded)
    const { runEmbedding, runVITS, releaseIdleTransformerModels } = await import('./transformers')
    await runEmbedding(['a'], undefined, 'wasm')
    await runVITS('text', 'synthetic/speech')
    await releaseIdleTransformerModels()
    await releaseIdleTransformerModels()
    expect(embedding.dispose).toHaveBeenCalledTimes(1)
    expect(speech.dispose).toHaveBeenCalledTimes(1)
    const audio = AudioContextFixture.instances[0]
    expect(audio.close).not.toHaveBeenCalled()
    expect(await runEmbedding(['b'], undefined, 'wasm')).toEqual([new Float32Array([2])])
    expect(harness.pipeline).toHaveBeenCalledTimes(3)
    audio.source.onended!()
    expect(audio.close).toHaveBeenCalledTimes(1)
})

it.each(['embedding', 'speech'] as const)('skips an active %s model without waiting for its inference', async (kind) => {
    const result = kind === 'embedding' ? { data: new Float32Array([1]) }
        : { sampling_rate: 16000, audio: new Float32Array([0]) }
    const pending = deferred<typeof result>()
    const instance = model(result)
    instance.mockReturnValueOnce(pending.promise)
    harness.pipeline.mockResolvedValue(instance)
    const { runEmbedding, runVITS, releaseIdleTransformerModels } = await import('./transformers')
    const inference = kind === 'embedding' ? runEmbedding(['a'], undefined, 'wasm') : runVITS('text')
    await vi.waitFor(() => expect(instance).toHaveBeenCalledTimes(1))
    await releaseIdleTransformerModels()
    expect(instance.dispose).not.toHaveBeenCalled()
    pending.resolve(result)
    await inference
    await releaseIdleTransformerModels()
    expect(instance.dispose).toHaveBeenCalledTimes(1)
    for (const audio of AudioContextFixture.instances) audio.source.onended!()
})

it('serializes synthesis before disposing a replaced speech model', async () => {
    const output = { sampling_rate: 16000, audio: new Float32Array([0]) }
    const pending = deferred<typeof output>()
    const first = model(output)
    first.mockReturnValueOnce(pending.promise)
    const second = model(output)
    harness.pipeline.mockResolvedValueOnce(first).mockResolvedValueOnce(second)
    const { runVITS } = await import('./transformers')
    const before = runVITS('first', 'synthetic/a')
    await vi.waitFor(() => expect(first).toHaveBeenCalledTimes(1))
    const after = runVITS('second', 'synthetic/b')
    await Promise.resolve()
    expect(first.dispose).not.toHaveBeenCalled()
    pending.resolve(output)
    await Promise.all([before, after])
    expect(first.dispose).toHaveBeenCalledTimes(1)
    for (const audio of AudioContextFixture.instances) audio.source.onended!()
})
