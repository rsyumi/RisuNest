import { MAX_CONTENT_METADATA_BYTES } from '../storage/contentImportLimits'
import {
    AppendableBuffer,
    saveAsset,
    type LocalWriter,
    type VirtualWriter,
} from '../globalApi.svelte'
import * as fflate from "fflate";
import { asBuffer, Semaphore, sleep } from "../util";
import { alertStore } from "../alert";
import { withObjectUrl } from "../objectUrl";

// File size and chunk size constants
const MAX_ASSET_SIZE_BYTES = 50 * 1024 * 1024; // 50MB
const CHUNK_SIZE_BYTES = 1024 * 1024; // 1MB

// Queue management constants
const MAX_CONCURRENT_ASSET_SAVES = 10;

// HTTP status code ranges

export async function processZip(dataArray: Uint8Array): Promise<string> {
    const unzipped = await new Promise<fflate.Unzipped>((resolve, reject) => {
        fflate.unzip(dataArray, (err, data) => {
            if (err) reject(err);
            else resolve(data);
        });
    });

    const imageFile = Object.keys(unzipped).find(fileName => /\.(jpg|jpeg|png)$/i.test(fileName));
    if (imageFile) {
        const imageData = unzipped[imageFile];
        const blob = new Blob([asBuffer(imageData)], { type: 'image/png' });
        const base64 = await new Promise<string>((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = () => {
                const result = reader.result as string;
                resolve(result);
            };
            reader.onerror = reject;
            reader.readAsDataURL(blob);
        });
        return base64;
    } else {
        throw new Error("No image found in ZIP file");
    }
}

export class CharXWriter{
    zip:fflate.Zip
    writeEnd:boolean = false
    apb = new AppendableBuffer()
    #takenFilenames:Set<string> = new Set()
    constructor(private writer:LocalWriter|WritableStreamDefaultWriter<Uint8Array>|VirtualWriter){
        const handlerAsync = (err:Error, dat:Uint8Array, final:boolean) => {
            if(dat){
                this.apb.append(dat)
            }
            if(final){
                this.writeEnd = true
            }
        }

        this.zip = new fflate.Zip()
        this.zip.ondata = handlerAsync
    }
    async init(){
        //do nothing, just to make compatible with other writer
    }

    async writeJpeg(img: Uint8Array){
        const canvas = document.createElement('canvas')
        const ctx = canvas.getContext('2d')
        if(!ctx){
            return
        }
        const imgBlob = new Blob([asBuffer(img)], {type: 'image/jpeg'})
        await withObjectUrl(imgBlob, async (imgURL) => {
            const imgElement = document.createElement('img')
            imgElement.src = imgURL
            await imgElement.decode()
            canvas.width = imgElement.width
            canvas.height = imgElement.height
            ctx.drawImage(imgElement, 0, 0)
            const blob = await (new Promise((res:BlobCallback) => {
                canvas.toBlob(res, 'image/jpeg')
            }))
            const buf = await blob.arrayBuffer()
            this.apb.append(new Uint8Array(buf))
        })
    }

    async write(key:string,data:Uint8Array|string, level?:0|1|2|3|4|5|6|7|8|9){
        key = this.#sanitizeZipFilename(key)
        let dat:Uint8Array
        if(typeof data === 'string'){
            dat = new TextEncoder().encode(data)
        }
        else{
            dat = data
        }
        this.writeEnd = false
        const file = new fflate.ZipDeflate(key, {
            level: level ?? 0
        });
        this.zip.add(file)
        file.push(dat, true)
        await this.writer.write(this.apb.buffer)
        this.apb.clear()
        if(this.writeEnd){
            await this.writer.close()
        }
        
    }

    #sanitizeZipFilename(filename:string) {
        let sanitized = filename.replace(/[<>:"\\|?*\x00-\x1F]/g, '_');
        sanitized = sanitized.replace(/[. ]+$/, '');
        const reservedNames = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(\..*)?$/i;
        if (reservedNames.test(sanitized)) {
            sanitized = '_' + sanitized;
        }
        if (!sanitized || sanitized === '.' || sanitized === '..') {
            sanitized = 'file_' + Date.now();
        }

        const splitName = sanitized.split('.');
        let baseName = splitName.slice(0, -1).join('.');
        const extension = splitName.length > 1 ? '.' + splitName[splitName.length - 1] : '';
        let counter = 1;
        let uniqueName = baseName + extension;
        while (this.#takenFilenames.has(uniqueName)) {
            uniqueName = `${baseName}_${counter}${extension}`;
            counter++;
        }
        
        this.#takenFilenames.add(uniqueName);
        return uniqueName;
    }

    async end(){
        this.zip.end()
        await this.writer.write(this.apb.buffer)
        this.apb.clear()
        if(this.writeEnd){
            await this.writer.close()
        }
    }
}

/**
 * Streaming importer for CharX (character export) files.
 *
 * CharX files are ZIP archives containing:
 * - card.json: Character card data (CCv3 format)
 * - module.risum: Optional module data (scripts, lorebook)
 * - assets/*: Image and other asset files
 *
 * This class reads and imports character data by:
 * - Processing ZIP streams incrementally to handle large files efficiently
 * - Parsing metadata (card.json, module.risum) synchronously
 * - Saving assets to storage concurrently (limited to prevent memory exhaustion)
 */
export class CharXImporter {
    // ZIP streaming parser
    unzip: fflate.Unzip

    // Asset save semaphore
    private semaphore: Semaphore
    private pending = new Set<Promise<void>>()
    private pendingBytes = 0
    private lastProgressAt = 0

    // Completion tracking
    private totalEnqueued: number = 0
    private totalCompleted: number = 0
    private isFinalized: boolean = false
    private completionResolver?: () => void
    private completionRejecter?: (error: Error) => void
    private completionPromise?: Promise<void>
    private completionSettled: boolean = false
    private errors: Error[] = []
    private onProgress?: (done: number, total: number) => void

    // Results: filename -> saved asset ID mapping
    assets: { [key: string]: string } = {}

    // Temporary buffers for accumulating file chunks during streaming
    assetBuffers: { [key: string]: AppendableBuffer } = {}

    // Files excluded due to size limits (> MAX_ASSET_SIZE_BYTES)
    excludedFiles: string[] = []

    // Extracted character card JSON content
    cardData: string | undefined

    // Extracted module binary data
    moduleData: Uint8Array | undefined

    // Configuration
    alertInfo: boolean = false // Show progress alerts to user

    constructor(private persistAsset: typeof saveAsset = saveAsset) {
        this.unzip = new fflate.Unzip()
        this.unzip.register(fflate.UnzipInflate)
        this.unzip.onfile = (file) => this.#handleFile(file)

        this.semaphore = new Semaphore(MAX_CONCURRENT_ASSET_SAVES)
        this.onProgress = (done, total) => {
            const now = performance.now()
            if (
                this.alertInfo &&
                (done === total || now - this.lastProgressAt >= 100)
            ) {
                this.lastProgressAt = now
                alertStore.set({
                    type: 'wait',
                    msg: `Loading... (Saving Assets ${done}/${total})`,
                })
            }
        }
    }

    /**
     * High-level method to parse ZIP data from various sources.
     *
     * Handles three input types:
     * - ReadableStream: Streams data chunks as they arrive
     * - Uint8Array: Automatically converted to stream
     * - File: Uses built-in stream() method
     *
     * After parse() completes:
     * - cardData and moduleData are immediately available
     * - Asset saving continues in the background
     * - MUST call done() to wait for all assets to finish saving
     *
     * Usage:
     * ```
     * await importer.parse(data)
     * const card = importer.cardData  // Available immediately
     * await importer.done()           // Wait for assets
     * await saveCharacter(card, importer.assets)
     * ```
     */
    async parse(data: Uint8Array | File | ReadableStream<Uint8Array>) {
        // Create completion promise at the start of parsing
        this.completionPromise = this.#awaitCompletion()
        void this.completionPromise.catch(() => {})

        // Convert all input types to ReadableStream for uniform processing
        const stream = this.#toStream(data)

        const reader = stream.getReader()
        try {
            while (true) {
                const { done, value } = await reader.read()
                if (value) await this.#feedChunk(value)
                if (done) {
                    await this.#feedChunk(new Uint8Array(0), true)
                    break
                }
            }
        } catch (error) {
            await reader.cancel().catch(() => {})
            await Promise.all(this.pending)
            throw error
        } finally {
            reader.releaseLock()
        }
    }

    /**
     * Feeds a chunk of ZIP data to the streaming parser.
     * When final=true, marks input as complete and finalizes the save queue.
     */
    async #feedChunk(data: Uint8Array, final: boolean = false) {
        // fflate discovers entries recursively inside a push. Bound both recursion
        // and the decoded bytes awaiting storage, even for highly compressible ZIPs.
        for (let offset = 0; offset < data.length; offset += 1024) {
            this.unzip.push(data.subarray(offset, offset + 1024), false)
            while (
                this.pending.size >= MAX_CONCURRENT_ASSET_SAVES ||
                this.pendingBytes >= 8 * 1024 * 1024
            ) {
                await Promise.race(this.pending)
            }
            if (this.errors.length) throw this.errors[0]
        }
        if (final) {
            this.unzip.push(new Uint8Array(0), true)
            this.#finalize()
        }
    }

    /**
     * Returns a promise that resolves when all assets have been processed.
     * Must be called after parse() has been invoked.
     */
    async done() {
        if (!this.completionPromise) {
            throw new Error('parse() must be called before done()')
        }
        return this.completionPromise
    }

    #awaitCompletion(): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            this.completionResolver = resolve
            this.completionRejecter = reject
            this.completionSettled = false
            this.errors = []
            this.#checkCompletion()
        })
    }

    #checkCompletion(): void {
        if (
            !this.completionSettled &&
            this.isFinalized &&
            this.totalCompleted >= this.totalEnqueued
        ) {
            this.completionSettled = true
            if (this.errors.length > 0) {
                const error =
                    this.errors.length === 1
                        ? this.errors[0]
                        : new AggregateError(
                              this.errors,
                              `Failed to save ${this.errors.length} assets`,
                          )
                this.completionRejecter?.(error)
                return
            }
            this.completionResolver?.()
        }
    }

    /**
     * Converts various data types to ReadableStream for uniform processing.
     */
    #toStream(
        data: Uint8Array | File | ReadableStream<Uint8Array>,
    ): ReadableStream<Uint8Array> {
        // Already a stream - return as-is
        if (data instanceof ReadableStream) {
            return data
        }

        // File has built-in stream() method
        if (data instanceof File) {
            return data.stream()
        }

        // Convert Uint8Array to stream, chunked to prevent blocking
        let offset = 0
        return new ReadableStream({
            pull(controller) {
                if (offset >= data.byteLength) {
                    controller.close()
                    return
                }
                const end = Math.min(offset + CHUNK_SIZE_BYTES, data.byteLength)
                controller.enqueue(data.subarray(offset, end))
                offset = end
            },
        })
    }

    /**
     * Called when a new file is discovered in the ZIP archive.
     * Sets up streaming handlers and starts processing if file size is acceptable.
     */
    #entryLimit(name: string): number {
        return name === 'card.json' || name === 'module.risum'
            ? MAX_CONTENT_METADATA_BYTES
            : MAX_ASSET_SIZE_BYTES
    }

    #handleFile(file: fflate.UnzipFile) {
        const assetIndex = file.name
        const limit = this.#entryLimit(assetIndex)
        this.assetBuffers[assetIndex] = new AppendableBuffer()

        file.ondata = (error, dat, final) => {
            if (error) throw error
            const buffer = this.assetBuffers[assetIndex]
            if (!buffer) return
            if (buffer.length() + dat.length > limit) {
                delete this.assetBuffers[assetIndex]
                this.excludedFiles.push(assetIndex)
                file.terminate()
                return
            }
            this.#handleFileData(assetIndex, dat, final)
        }

        // Only process files smaller than MAX_ASSET_SIZE_BYTES (50MB)
        if ((file.originalSize ?? 0) <= limit) {
            file.start()
        } else {
            delete this.assetBuffers[assetIndex]
            this.excludedFiles.push(assetIndex)
        }
    }

    /**
     * Called for each chunk of file data as it streams in.
     * Accumulates chunks into buffer until file is complete.
     */
    #handleFileData(fileName: string, data: Uint8Array, final: boolean) {
        this.assetBuffers[fileName].append(data)
        if (final) {
            this.#handleFileComplete(fileName)
        }
    }

    /**
     * Called when a file has been completely read from the ZIP.
     * Routes files to appropriate handlers based on filename/extension.
     */
    #handleFileComplete(fileName: string) {
        const assetData = this.assetBuffers[fileName].buffer

        if (assetData.byteLength > this.#entryLimit(fileName)) {
            this.excludedFiles.push(fileName)
        } else if (fileName === 'card.json') {
            this.cardData = new TextDecoder().decode(assetData)
        } else if (fileName === 'module.risum') {
            this.moduleData = assetData
        } else if (fileName.endsWith('.json')) {
            // Ignore other JSON files
        } else {
            // All other files are treated as assets (images, etc.)
            this.pendingBytes += assetData.byteLength
            const pending = this.#processAssetQueue({
                id: fileName,
                data: assetData,
            })
            this.pending.add(pending)
            void pending.then(() => {
                this.pending.delete(pending)
                this.pendingBytes -= assetData.byteLength
            })
        }

        delete this.assetBuffers[fileName]
    }

    /**
     * Queues an asset for saving with concurrency control.
     */
    async #processAssetQueue(asset: { id: string; data: Uint8Array }) {
        this.totalEnqueued += 1
        let acquired = false
        try {
            await this.semaphore.acquire()
            acquired = true
            this.assets[asset.id] = await this.persistAsset(asset.data)
        } catch (error) {
            this.errors.push(
                error instanceof Error ? error : new Error(String(error)),
            )
        } finally {
            if (acquired) {
                this.semaphore.release()
            }
            this.totalCompleted += 1
            this.onProgress?.(this.totalCompleted, this.totalEnqueued)
            this.#checkCompletion()
        }
    }

    /**
     * Finalizes processing when all ZIP data has been pushed.
     */
    #finalize() {
        this.isFinalized = true
        this.#checkCompletion()
    }
}


