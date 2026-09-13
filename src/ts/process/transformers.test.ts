import { afterEach, beforeEach, expect, it, vi } from 'vitest'

const harness = vi.hoisted(() => ({
    load: vi.fn(),
    pipeline: vi.fn(),
    env: {} as Record<string, unknown>,
}))

vi.mock('@huggingface/transformers', () => ({
    AutoTokenizer: { from_pretrained: harness.load },
    pipeline: harness.pipeline,
    env: harness.env,
}))
vi.mock('src/ts/globalApi.svelte', () => ({
    loadAsset: vi.fn(),
    saveAsset: vi.fn(),
}))
vi.mock('src/ts/util', () => ({
    selectSingleFile: vi.fn(),
    asBuffer: (data: unknown) => data,
}))

beforeEach(() => {
    vi.resetModules()
    harness.load.mockReset()
    harness.pipeline.mockReset()
    vi.stubGlobal('caches', {
        open: vi.fn().mockResolvedValue({ put: vi.fn(), match: vi.fn() }),
    })
})

afterEach(() => vi.unstubAllGlobals())

it('uses the selected model tokenizer and shares the generation cache without loading model weights', async () => {
    const encode = vi.fn().mockReturnValue([1, 42, 2])
    harness.load.mockResolvedValue({ encode })
    const { tokenizeTransformers } = await import('./transformers')

    await expect(
        tokenizeTransformers('synthetic text', 'synthetic/model-a'),
    ).resolves.toEqual([1, 42, 2])
    await tokenizeTransformers('another text', 'synthetic/model-a')

    expect(harness.load).toHaveBeenCalledExactlyOnceWith('synthetic/model-a')
    expect(encode).toHaveBeenNthCalledWith(1, 'synthetic text')
    expect(encode).toHaveBeenNthCalledWith(2, 'another text')
    expect(harness.env.useCustomCache).toBe(true)
    expect(harness.env.customCache).toBeDefined()
    expect(harness.pipeline).not.toHaveBeenCalled()
})

it('shares an in-flight tokenizer download for concurrent calls', async () => {
    let resolve!: (tokenizer: { encode: (text: string) => number[] }) => void
    harness.load.mockReturnValue(
        new Promise((done) => {
            resolve = done
        }),
    )
    const { tokenizeTransformers } = await import('./transformers')
    const first = tokenizeTransformers('first', 'synthetic/model-a')
    const second = tokenizeTransformers('second', 'synthetic/model-a')
    await vi.waitFor(() => expect(harness.load).toHaveBeenCalledTimes(1))
    resolve({ encode: (text) => [text.length] })
    await expect(Promise.all([first, second])).resolves.toEqual([[5], [6]])
})

it('uses each requested model when downloads finish out of order', async () => {
    let resolveFirst!: (tokenizer: { encode: () => number[] }) => void
    harness.load.mockReturnValueOnce(
        new Promise((done) => {
            resolveFirst = done
        }),
    )
    harness.load.mockResolvedValueOnce({ encode: () => [22] })
    const { tokenizeTransformers } = await import('./transformers')
    const first = tokenizeTransformers('text', 'synthetic/model-a')
    await vi.waitFor(() => expect(harness.load).toHaveBeenCalledTimes(1))
    await expect(
        tokenizeTransformers('text', 'synthetic/model-b'),
    ).resolves.toEqual([22])
    resolveFirst({ encode: () => [11] })
    await expect(first).resolves.toEqual([11])
})

it('reports download failure and permits a later retry', async () => {
    harness.load.mockRejectedValueOnce(new Error('synthetic download failure'))
    harness.load.mockResolvedValueOnce({ encode: () => [42] })
    const { tokenizeTransformers } = await import('./transformers')
    await expect(
        tokenizeTransformers('text', 'synthetic/model-a'),
    ).rejects.toThrow('synthetic download failure')
    await expect(
        tokenizeTransformers('text', 'synthetic/model-a'),
    ).resolves.toEqual([42])
    expect(harness.load).toHaveBeenCalledTimes(2)
})
