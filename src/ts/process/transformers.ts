import type { SummarizationOutput, TextToAudioPipeline, FeatureExtractionPipeline, TextGenerationConfig, TextGenerationOutput, ImageToTextOutput } from '@huggingface/transformers';
import { unzip } from 'fflate';
import { loadAsset, saveAsset } from 'src/ts/globalApi.svelte';
import { selectSingleFile, asBuffer  } from 'src/ts/util';
import { v4 } from 'uuid';
import type { PreTrainedTokenizer } from '@huggingface/transformers';
import { Mutex } from '../mutex';
import { getRuntimePerformanceBudgets } from '../runtimePerformanceProfile';

const initializationMutex = new Mutex()
const embeddingMutex = new Mutex()
const synthesisMutex = new Mutex()

async function disposePipeline(pipeline: { dispose(): Promise<void> }): Promise<void> {
    try {
        await pipeline.dispose()
    } catch {
        // Cleanup must not replace an inference result or its original error.
        console.warn('Local model resource cleanup failed')
    }
}
let tfCache: Cache = null
let tfLoaded = false
let tfMap: { [key: string]: string } = {}
async function initTransformers() {
    return initializationMutex.runExclusive(configureTransformers)
}

async function configureTransformers() {
    if (tfLoaded) {
        return
    }
    const { env } = await import('@huggingface/transformers');
    tfCache = await caches.open('tfCache')
    env.localModelPath = "https://sv.risuai.xyz/transformers/"
    env.useBrowserCache = false
    env.useFSCache = false
    env.useCustomCache = true
    env.allowLocalModels = true
    env.customCache = {
        put: async (url: URL | string, response: Response) => {
            await tfCache.put(url, response)
        },
        match: async (url: URL | string) => {
            if (typeof url === 'string') {
                if (Object.keys(tfMap).includes(url)) {
                    const assetId = tfMap[url]
                    return new Response(asBuffer(await loadAsset(assetId)))
                }
            }
            return await tfCache.match(url)
        }
    }
    tfLoaded = true
    console.log('transformers loaded')
}

let textTokenizer:
    | { model: string; loaded: Promise<PreTrainedTokenizer> }
    | undefined

export async function tokenizeTransformers(
    text: string,
    model: string,
): Promise<number[]> {
    await initTransformers()
    const { AutoTokenizer } = await import('@huggingface/transformers')
    if (textTokenizer?.model !== model) {
        const loaded = AutoTokenizer.from_pretrained(model)
        textTokenizer = { model, loaded }
        // Keep one model resident and allow a failed download to be retried.
        void loaded.catch(() => {
            if (textTokenizer?.loaded === loaded) textTokenizer = undefined
        })
    }
    const tokenizer = await textTokenizer.loaded
    return tokenizer.encode(text)
}

export const runTransformers = async (baseText: string, model: string, config: TextGenerationConfig, device: 'webgpu' | 'wasm' = 'wasm') => {
    await initTransformers()
    const { pipeline } = await import('@huggingface/transformers');
    const generator = await pipeline('text-generation', model, { device });
    try {
        const output = await generator(baseText, config) as TextGenerationOutput
        return output[0]
    } finally {
        await disposePipeline(generator)
    }
}

export const runSummarizer = async (text: string) => {
    await initTransformers()
    const { pipeline } = await import('@huggingface/transformers');
    const classifier = await pipeline("summarization", "Xenova/distilbart-cnn-6-6")
    try {
        const result = await classifier(text) as SummarizationOutput
        return result[0].summary_text
    } finally {
        await disposePipeline(classifier)
    }
}

let extractor: FeatureExtractionPipeline = null
let lastEmbeddingModelQuery: string = ''
type EmbeddingModel = 'Xenova/all-MiniLM-L6-v2' | 'nomic-ai/nomic-embed-text-v1.5'
export const runEmbedding = async (texts: string[], model: EmbeddingModel = 'Xenova/all-MiniLM-L6-v2', device: 'webgpu' | 'wasm'): Promise<Float32Array[]> => {
    if (texts.length === 0) return []
    return embeddingMutex.runExclusive(async () => {
        await initTransformers()
        const embeddingModelQuery = model + device
        const { pipeline } = await import('@huggingface/transformers');
        if (!extractor || embeddingModelQuery !== lastEmbeddingModelQuery) {
            const previous = extractor
            extractor = null
            lastEmbeddingModelQuery = ''
            if (previous) await disposePipeline(previous)
            extractor = await pipeline<"feature-extraction">('feature-extraction', model, {
                // Default dtype for webgpu is fp32, so we can use q8, which is the default dtype in wasm.
                dtype: "q8",
                device,
                progress_callback: (progress) => { console.log(progress) },
            });
            lastEmbeddingModelQuery = embeddingModelQuery
        }
        const vectors: Float32Array[] = []
        const batchSize = Math.min(texts.length, getRuntimePerformanceBudgets().localEmbeddingBatchEntries)
        for (let offset = 0; offset < texts.length; offset += batchSize) {
            const batch = texts.slice(offset, offset + batchSize)
            const result = await extractor(batch, { pooling: 'mean', normalize: true });
            const data = result.data as Float32Array
            const lenPerText = data.length / batch.length
            for (let i = 0; i < batch.length; i++) {
                vectors.push(data.subarray(i * lenPerText, (i + 1) * lenPerText))
            }
        }
        return vectors
    })
}

export const runImageEmbedding = async (dataurl: string) => {
    await initTransformers()
    const { pipeline } = await import('@huggingface/transformers');
    const captioner = await pipeline('image-to-text', 'Xenova/vit-gpt2-image-captioning');
    try {
        return await captioner(dataurl) as ImageToTextOutput
    } finally {
        await disposePipeline(captioner)
    }
}

let synthesizer: TextToAudioPipeline = null
let lastSynth: string = null

export async function releaseIdleTransformerModels(): Promise<void> {
    const releases: Promise<void>[] = []
    if (!embeddingMutex.isLocked) {
        releases.push(embeddingMutex.runExclusive(async () => {
            const previous = extractor
            extractor = null
            lastEmbeddingModelQuery = ''
            if (previous) await disposePipeline(previous)
        }))
    }
    if (!synthesisMutex.isLocked) {
        releases.push(synthesisMutex.runExclusive(async () => {
            const previous = synthesizer
            synthesizer = null
            lastSynth = null
            tfMap = {}
            if (previous) await disposePipeline(previous)
        }))
    }
    await Promise.all(releases)
}

export interface OnnxModelFiles {
    files: { [key: string]: string },
    id: string,
    name?: string
}

export const runVITS = async (text: string, modelData: string | OnnxModelFiles = 'Xenova/mms-tts-eng') => {
    if (modelData === null) return
    const audio = await synthesisMutex.runExclusive(async () => {
        await initTransformers()
        const { WaveFile } = await import('wavefile')
        const { pipeline, env } = await import('@huggingface/transformers');
        const model = typeof modelData === 'string' ? modelData : modelData.id
        if (!synthesizer || lastSynth !== model) {
            const previous = synthesizer
            synthesizer = null
            lastSynth = null
            if (previous) await disposePipeline(previous)
            tfMap = {}
            if (typeof modelData !== 'string') {
                for (const [key, assetId] of Object.entries(modelData.files)) {
                    const fileURL = env.localModelPath + model + '/' + key
                    tfMap[fileURL] = assetId
                    tfMap[location.origin + fileURL] = assetId
                }
            }
            synthesizer = await pipeline<"text-to-speech">('text-to-speech', model)
            lastSynth = model
        }
        const output = await synthesizer(text, {})
        const wav = new WaveFile()
        wav.fromScratch(1, output.sampling_rate, '32f', output.audio)
        return new Uint8Array(wav.toBuffer()).buffer
    })
    await playSynthesizedAudio(audio)
}

async function playSynthesizedAudio(audio: ArrayBuffer): Promise<void> {
    const context = new AudioContext()
    let source: AudioBufferSourceNode | undefined
    let released = false
    const release = async () => {
        if (released) return
        released = true
        if (source) {
            source.onended = null
            try { source.disconnect() } catch {}
            source.buffer = null
            source = undefined
        }
        try { await context.close() } catch {
            console.warn('Speech audio resource cleanup failed')
        }
    }
    try {
        const decoded = await context.decodeAudioData(audio)
        source = context.createBufferSource()
        source.buffer = decoded
        source.onended = () => { void release() }
        source.connect(context.destination)
        source.start()
    } catch (error) {
        await release()
        throw error
    }
}

export const registerOnnxModel = async (): Promise<OnnxModelFiles> => {
    const id = v4().replace(/-/g, '')

    const modelFile = await selectSingleFile(['zip'])

    if (!modelFile) {
        return
    }

    const unziped = await new Promise((res, rej) => {
        unzip(modelFile.data, {
            filter: (file) => {
                return file.name.endsWith('.onnx') || file.size < 10_000_000 || file.name.includes('.git')
            }
        }, (err, unzipped) => {
            if (err) {
                rej(err)
            }
            else {
                res(unzipped)
            }
        })
    })

    let fileIdMapped: { [key: string]: string } = {}

    const keys = Object.keys(unziped)
    for (let i = 0; i < keys.length; i++) {
        const key = keys[i]
        const file = unziped[key]
        const fid = await saveAsset(file)
        let url = key
        if (url.startsWith('/')) {
            url = url.substring(1)
        }
        fileIdMapped[url] = fid
    }

    return {
        files: fileIdMapped,
        name: modelFile.name,
        id: id,
    }

}
