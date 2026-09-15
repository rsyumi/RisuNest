import { globalFetch } from "src/ts/globalApi.svelte";
import { runEmbedding } from "../transformers";
import { appendLastPath } from "src/ts/util";
import { getDatabase } from "src/ts/storage/database.svelte";
import {
    consistentEmbeddings,
    getHypaEmbeddingCache,
    staleEmbeddingKeys,
    toVectorBuffer,
    type HypaEmbeddingEntry,
} from "src/ts/storage/hypaEmbeddingCache";
import { isContextModel, getContextProvider } from "./contextualEmbedding";
import {
    HYPA_PREPROCESS_VERSION,
    hypaCacheKeys,
    hypaEmbeddingIdentity,
    type HypaCacheProducer,
    type HypaEmbeddingIdentity,
} from "./hypaCacheKey";

export type HypaModel = 'custom'|'ada'|'openai3small'|'openai3large'|'MiniLM'|'MiniLMGPU'|'nomic'|'nomicGPU'|'bgeSmallEn'|'bgeSmallEnGPU'|'bgem3'|'bgem3GPU'|'multiMiniLM'|'multiMiniLMGPU'|'bgeM3Ko'|'bgeM3KoGPU'|'voyageContext3'

// In a typical environment, bge-m3 is a heavy model.
// If your GPU can't handle this model, you'll see errror below.
// Failed to execute 'mapAsync' on 'GPUBuffer': [Device] is lost
export const localModels = {
    models: {
        'MiniLM':'Xenova/all-MiniLM-L6-v2',
        'MiniLMGPU': "Xenova/all-MiniLM-L6-v2",
        'nomic':'nomic-ai/nomic-embed-text-v1.5',
        'nomicGPU':'nomic-ai/nomic-embed-text-v1.5',
        'bgeSmallEn': 'Xenova/bge-small-en-v1.5',
        'bgeSmallEnGPU': 'Xenova/bge-small-en-v1.5',
        'bgem3': 'Xenova/bge-m3',
        'bgem3GPU': 'Xenova/bge-m3',
        'multiMiniLM': 'Xenova/paraphrase-multilingual-MiniLM-L12-v2',
        'multiMiniLMGPU': 'Xenova/paraphrase-multilingual-MiniLM-L12-v2',
        'bgeM3Ko': 'HyperBlaze/BGE-m3-ko',
        'bgeM3KoGPU': 'HyperBlaze/BGE-m3-ko',
    },
    gpuModels:[
        'MiniLMGPU',
        'nomicGPU',
        'bgeSmallEnGPU',
        'bgem3GPU',
        'multiMiniLMGPU',
        'bgeM3KoGPU',
    ]
}

export class HypaProcesser{
    oaikey:string
    vectors:memoryVector[]
    model:HypaModel
    customEmbeddingUrl:string

    constructor(model:HypaModel|'auto' = 'auto',customEmbeddingUrl?:string){
        this.vectors = []
        const db = getDatabase()
        if(model === 'auto'){
            this.model = db.hypaModel || 'MiniLM'
        }
        else{
            this.model = model
        }
        this.customEmbeddingUrl = customEmbeddingUrl?.trim() || db.hypaCustomSettings?.url?.trim() || ""
    }

    async embedDocuments(texts: string[]): Promise<VectorArray[]> {
        const subPrompts = chunkArray(texts,50);
    
        const embeddings: VectorArray[] = [];
    
        for (let i = 0; i < subPrompts.length; i += 1) {
          const input = subPrompts[i];
    
          const data = await this.getEmbeds(input, 'document')
    
          embeddings.push(...data);
        }
    
        return embeddings;
    }
    
    
    async getEmbeds(input:string[]|string, inputType:'query'|'document' = 'query'):Promise<VectorArray[]> {
        if(isContextModel(this.model)){
            const provider = getContextProvider(this.model)
            const inputs:string[] = Array.isArray(input) ? input : [input]
            if(inputType === 'query'){
                return await provider.embedQueries(inputs)
            }
            const groups = inputs.map(s => [s])
            const results = await provider.embedDocumentGroups(groups)
            return results.map(group => group[0])
        }
        if(Object.keys(localModels.models).includes(this.model)){
            const inputs:string[] = Array.isArray(input) ? input : [input]
            let results:Float32Array[] = await runEmbedding(inputs, localModels.models[this.model], localModels.gpuModels.includes(this.model) ? 'webgpu' : 'wasm')
            return results
        }
        let gf = null;
        if(this.model === 'custom'){
            if(!this.customEmbeddingUrl){
                throw new Error('Custom model requires a Custom Server URL')
            }
            const {customEmbeddingUrl} = this
            const replaceUrl = customEmbeddingUrl.endsWith('/embeddings')?customEmbeddingUrl:appendLastPath(customEmbeddingUrl,'embeddings')

            const db = getDatabase()
            const fetchArgs = {
                headers: {
                    ...(db.hypaCustomSettings?.key?.trim() ? {"Authorization": "Bearer " + db.hypaCustomSettings.key.trim()} : {})
                },
                body: {
                    "input": input,
                    ...(db.hypaCustomSettings?.model?.trim() ? {"model": db.hypaCustomSettings.model.trim()} : {})
                }
            };
 
            gf = await globalFetch(replaceUrl.toString(), fetchArgs)
        }
        if(this.model === 'ada' || this.model === 'openai3small' || this.model === 'openai3large'){
            const db = getDatabase()
            const models = {
                'ada':'text-embedding-ada-002',
                'openai3small':'text-embedding-3-small',
                'openai3large':'text-embedding-3-large'
            }

            gf = await globalFetch("https://api.openai.com/v1/embeddings", {
                headers: {
                    "Authorization": "Bearer " + (this.oaikey?.trim() || db.supaMemoryKey?.trim())
                },
                body: {
                    "input": input,
                    "model": models[this.model]
                }
            })
        }
        const data = gf.data
    
    
        if(!gf.ok){
            throw JSON.stringify(gf.data)
        }
    
        const result:number[][] = []
        for(let i=0;i<data.data.length;i++){
            result.push(data.data[i].embedding)
        }
    
        return result
    }

    protected cacheIdentity():HypaEmbeddingIdentity{
        const db = getDatabase()
        return hypaEmbeddingIdentity(this.model, this.customEmbeddingUrl, db.hypaCustomSettings?.model)
    }

    protected cacheEntry(producer:HypaCacheProducer, key:string, embedding:VectorArray):HypaEmbeddingEntry{
        const identity = this.cacheIdentity()
        return {
            key,
            producer,
            model: identity.model,
            endpoint: identity.endpoint || null,
            preprocessVersion: HYPA_PREPROCESS_VERSION,
            dimensions: embedding.length,
            vector: toVectorBuffer(embedding),
        }
    }

    async addText(texts:string[]) {
        const identity = this.cacheIdentity()
        const cache = getHypaEmbeddingCache()
        const keys = await hypaCacheKeys(
            texts.map((content) => ({ producer: 'hypa-v1-text' as const, content, identity }))
        )
        const keyOf = new Map(texts.map((text, index) => [text, keys[index]]))
        const cached = consistentEmbeddings(await cache.read(keys))

        for(let i=0;i<texts.length;i++){
            const hit = cached.get(keys[i])
            if(hit){
                this.vectors.push({
                    content: texts[i],
                    embedding: hit.vector,
                    alreadySaved: true
                })
            }
        }

        let pending = texts.filter((v) => {
            for(let i=0;i<this.vectors.length;i++){
                if(this.vectors[i].content === v){
                    return false
                }
            }
            return true
        })

        if(pending.length === 0){
            return
        }
        let vectors = await this.embedDocuments(pending)

        const stale = new Set(staleEmbeddingKeys(cached, vectors[0]?.length ?? 0))
        if(stale.size > 0){
            const recompute = texts.filter((text) => stale.has(keyOf.get(text)))
            const recomputed = new Set(recompute)
            this.vectors = this.vectors.filter((vector) => !recomputed.has(vector.content))
            pending = pending.concat(recompute)
            vectors = vectors.concat(await this.embedDocuments(recompute))
        }

        const memoryVectors:memoryVector[] = vectors.map((embedding, idx) => ({
            content: pending[idx],
            embedding
        }));

        await cache.write(memoryVectors.map((vec) =>
            this.cacheEntry('hypa-v1-text', keyOf.get(vec.content), vec.embedding)
        ))

        this.vectors = memoryVectors.concat(this.vectors)
    }

    async similaritySearch(query: string) {
        const results = await this.similaritySearchVectorWithScore((await this.getEmbeds(query))[0],);
        return results.map((result) => result[0]);
    }

    async similaritySearchScored(query: string) {
        return await this.similaritySearchVectorWithScore((await this.getEmbeds(query))[0],);
    }

    private similaritySearchVectorWithScore(
        query: VectorArray,
      ): [string, number][] {
          const memoryVectors = this.vectors
          const sim = similarity
          const searches = memoryVectors
                .map((vector, index) => ({
                    similarity: sim(query, vector.embedding),
                    index,
                }))
                .sort((a, b) => (a.similarity > b.similarity ? -1 : 0))
      
          const result: [string, number][] = searches.map((search) => [
              memoryVectors[search.index].content,
              search.similarity,
          ]);
      
          return result;
    }

    similarityCheck(query1:number[],query2: number[]) {
        return similarity(query1, query2)
    }
}

export function similarity(a:VectorArray, b:VectorArray) {
    let dot = 0;
    for(let i=0;i<a.length;i++){
        dot += a[i] * b[i]
    }
    return dot
}

export function cosineSimilarity(a:VectorArray, b:VectorArray):number {
    let dot = 0;
    let magA = 0;
    let magB = 0;
    for(let i=0;i<a.length;i++){
        dot += a[i] * b[i];
        magA += a[i] * a[i];
        magB += b[i] * b[i];
    }
    return dot / (Math.sqrt(magA) * Math.sqrt(magB));
}

export function contextHash(texts: string[]): string {
    let h = 0x811c9dc5
    const s = texts.join('\0')
    for (let i = 0; i < s.length; i++) {
        h ^= s.charCodeAt(i)
        h = Math.imul(h, 0x01000193)
    }
    return (h >>> 0).toString(36)
}

export type VectorArray = number[]|Float32Array

export type memoryVector = {
    embedding:number[]|Float32Array,
    content:string,
    alreadySaved?:boolean
}

const chunkArray = <T>(arr: T[], chunkSize: number) =>
    arr.reduce((chunks, elem, index) => {
        const chunkIndex = Math.floor(index / chunkSize);
        const chunk = chunks[chunkIndex] || [];
        chunks[chunkIndex] = chunk.concat([elem]);
        return chunks;
}, [] as T[][]);
