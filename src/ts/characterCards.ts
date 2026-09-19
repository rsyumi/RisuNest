import { runContentImport } from './storage/contentImportOperation'
import { writable, type Writable } from 'svelte/store'
import { alertCardExport, alertConfirm, alertError, alertInput, alertMd, alertNormal, alertRisuServiceTOS, alertStore, alertWait } from "./alert"
import { defaultSdDataFunc, type character, setDatabase, type customscript, type loreSettings, type loreBook, type triggerscript, importPreset, type groupChat, setCurrentCharacter, getCurrentCharacter, getDatabase, setDatabaseLite, appVer } from "./storage/database.svelte"
import { checkNullish, decryptBuffer, isKnownUri, selectFileByDom, sleep } from "./util"
import { language } from "src/lang"
import { v4 as uuidv4, v4 } from 'uuid';
import { changeChar, characterFormatUpdate, commitDetachedCharacter } from "./characters"
import { AppendableBuffer, BlankWriter, checkCharOrder, downloadFile, loadAsset, LocalWriter, openURL, readImage, saveAsset, VirtualWriter } from "./globalApi.svelte"
import { isTauri, isNodeServer, isTauriDesktop, isTauriAndroid, isTauriIOS } from "src/ts/platform"
import { getImageType } from "./media"
import { DBState, SettingsMenuIndex, ShowRealmFrameStore, selectedCharID, settingsOpen } from "./stores.svelte"
import { hasher } from "./parser/parser.svelte"
import { type CharacterCardV3, type LorebookEntry } from '@risuai/ccardlib'
import { reencodeImage } from "./process/files/inlays"
import { PngChunk } from "./pngChunk"
import type { OnnxModelFiles } from "./process/transformers"
import { CharXImporter, CharXWriter } from "./process/processzip"
import { exportModuleLegacy, readModule, type RisuModule } from "./process/modules"
import { readFile } from "@tauri-apps/plugin-fs"
import { open } from "@tauri-apps/plugin-dialog"
import { REALM_HUB_URL, REALM_NIGHTLY_HUB_URL, REALM_NODE_PROXY_BASE, REALM_SITE_URL } from "./realmEndpoints"
import { registerOpenedFileListeners } from "./openedFiles"
import { type DeviceMarkerStorage } from './storage/deviceMarkers'
import { importDesktopNativeCharacterPath } from './storage/nativeCharacterFileRoute'
import type { NativeFileJobOptions, NativeFileJobSource } from './storage/nativeFileJobs'
import {
    decodePreparedNativePngCardMetadata,
    type PreparedNativePngCardMetadata,
} from './storage/nativePngCardAdapter'
import { exportNativeCharacterCharxFromPicker } from './storage/nativeCharacterCharxExportRoute'
import { exportNativeCharacterCardFromPicker } from './storage/nativeCharacterCardExportRoute'


const EXTERNAL_HUB_URL = 'https://sv.risuai.xyz';
const NIGHTLY_HUB_URL = 'https://nightly.sv.risuai.xyz'
const nightlyBuild = import.meta.env.VITE_RISU_NIGHTLY_BUILD === 'TRUE'
// The hub choice belongs to the device file, which only opens once the start
// reaches it, so the start applies the choice before anything requests a hub.
export let hubURL = isNodeServer
    ? '/hub-proxy'
    : nightlyBuild
    ? NIGHTLY_HUB_URL
    : EXTERNAL_HUB_URL;

// Only RisuRealm paths on this host go through `realmEndpoints`. Account login and
// Drive callbacks keep using `hubURL`, so agent mode does not affect them.
// `/rs/` serves both account and Realm-shared assets, so it is classified as Realm
// by scripts/realmBlocklist.mjs. Build every `/rs/` URL from this value.
export let realmHubURL = isNodeServer
    ? REALM_NODE_PROXY_BASE
    : nightlyBuild
    ? REALM_NIGHTLY_HUB_URL
    : REALM_HUB_URL;

export function applyHubSelection(markers: DeviceMarkerStorage): void {
    if (isNodeServer) return
    const nightly = nightlyBuild || markers.getItem('hub') === 'nightly'
    hubURL = nightly ? NIGHTLY_HUB_URL : EXTERNAL_HUB_URL
    realmHubURL = nightly ? REALM_NIGHTLY_HUB_URL : REALM_HUB_URL
}

const nativeCharacterContentImportEnabled = true

export function isNativeCharacterContentImportEnabled(): boolean {
    return nativeCharacterContentImportEnabled
}

export async function importPreparedNativeCharacterContent(
    input: {
        source: NativeFileJobSource
        displayName: string
    },
    options: NativeFileJobOptions = {},
): Promise<{ kind: 'declined' } | { kind: 'imported'; value: string }> {
    const [
        { runNativePreparedContentRoute },
        { activatePreparedNativeCharacterContent },
    ] = await Promise.all([
        import('./storage/nativePreparedContentRoute'),
        import('./storage/nativeCharacterContentActivation'),
    ])
    const result = await runContentImport(
        input.displayName,
        options,
        (operationOptions) =>
            runNativePreparedContentRoute(
                input.source,
                input.displayName,
                {
                    map: async (content) => content,
                    activate: (content, lifecycle, signal) =>
                        activatePreparedNativeCharacterContent(
                            content,
                            lifecycle,
                            undefined,
                            signal,
                        ),
                },
                operationOptions,
            ),
    )
    return result
        ? { kind: 'imported', value: result.characterId }
        : { kind: 'declined' }
}

export async function importCharacter() {
    let lastImportedCharacterId: string | null = null
    try {
        if (isTauriIOS) {
            const { importIOSContentFromPicker } = await import('./storage/iosContentPicker')
            return await importIOSContentFromPicker('character')
        }
        if (isTauriAndroid) {
            const { importAndroidContentFromPicker } = await import(
                './storage/androidContentPicker'
            )
            return await importAndroidContentFromPicker('character')
        }
        if (isTauriDesktop) {
            const selected = await open({ multiple: true, directory: false })
            const paths =
                typeof selected === 'string' ? [selected] : (selected ?? [])
            const importErrors: Array<string | Error> = []
            for (const path of paths) {
                try {
                    const result = await importDesktopNativeCharacterPath(path, {
                        readDesktopPath: readFile,
                        nativeEnabled: () => nativeCharacterContentImportEnabled,
                        nativeImport: importPreparedNativeCharacterContent,
                        legacyImport: async ({ name, data }) => {
                            const importedIndex = await importCharacterProcess({
                                name,
                                data,
                            })
                            return typeof importedIndex === 'number'
                                ? (getDatabase().characters[importedIndex]?.chaId ??
                                      null)
                                : null
                        },
                    })
                    if (result.kind === 'imported') {
                        checkCharOrder()
                        lastImportedCharacterId = result.value
                    }
                    if (result.kind === 'destination-required') {
                        alertError(
                            'This JPEG is not a character card. Choose an asset destination to import it.',
                        )
                    }
                } catch (error) {
                    importErrors.push(error instanceof Error ? error : String(error))
                }
            }
            const firstImportError = importErrors[0]
            if (firstImportError && importErrors.length === 1) alertError(firstImportError)
            if (importErrors.length > 1) {
                alertError(new AggregateError(
                    importErrors,
                    `Failed to import ${importErrors.length} of ${paths.length} selected character files`,
                ))
            }
            return lastImportedCharacterId
        }

        const files = await selectFileByDom(['*'], 'multiple')
        if (!files) {
            return
        }

        for (const f of files) {
            const importedIndex = await importCharacterProcess({
                name: f.name,
                data: f,
            })
            lastImportedCharacterId =
                getDatabase().characters[importedIndex]?.chaId ??
                lastImportedCharacterId
            checkCharOrder()
        }
        return lastImportedCharacterId
    } catch (error) {
        alertError(error)
        return null
    }
}

export async function importCharacterProcess<T extends boolean = false>(f: {
    name: string
    data: Uint8Array | File | ReadableStream<Uint8Array>
    lightningRealmImport?: boolean
    returnCharacter?: T //note That this option only works with v3 charx
}): Promise<T extends true ? character | number | null : number | null> {
    const fileName = f.name.split(/[\\/]/).at(-1) ?? f.name
    const separator = fileName.lastIndexOf('.')
    const extension =
        separator < 0 ? '' : fileName.slice(separator + 1).toLowerCase()
    if (
        isTauri &&
        !f.returnCharacter &&
        typeof File !== 'undefined' &&
        f.data instanceof File &&
        ['png', 'charx', 'jpg', 'jpeg'].includes(extension)
    ) {
        const { importNativeContentFile } = await import(
            './storage/nativeContentFile'
        )
        const id = await importNativeContentFile(f.data, 'character')
        return (
            id
                ? getDatabase().characters.findIndex(
                      (character) => character.chaId === id,
                  )
                : null
        ) as any
    }
    if (extension === 'json') {
        if (f.data instanceof ReadableStream) {
            return null
        }
        const data =
            f.data instanceof Uint8Array
                ? f.data
                : new Uint8Array(await f.data.arrayBuffer())
        const da = JSON.parse(Buffer.from(data).toString('utf-8'))
        const importedId = await importCharacterCardSpec(da)
        if (importedId) {
            return getDatabase().characters.findIndex(
                (character) => character.chaId === importedId,
            ) as any
        }
        if (
            (da.char_name || da.name) &&
            (da.char_persona || da.description) &&
            (da.char_greeting || da.first_mes)
        ) {
            const importedId = await commitDetachedCharacter(
                convertOffSpecCards(da),
                'import-character-card',
            )
            alertNormal(language.importedCharacter)
            return getDatabase().characters.findIndex(
                (character) => character.chaId === importedId,
            ) as any
        } else {
            alertError(language.errors.noData)
            return
        }
    }
    let db = getDatabase()
    db.statics.imports += 1

    if (extension === 'charx' || extension === 'jpg' || extension === 'jpeg') {
        console.log('reading charx')
        alertStore.set({
            type: 'wait',
            msg: 'Loading... (Reading)',
        })

        const importer = new CharXImporter()
        importer.alertInfo = true
        await importer.parse(f.data)
        const cardData = importer.cardData
        if (!cardData) {
            alertError(language.errors.noData)
            return
        }
        const card: CharacterCardV3 = JSON.parse(cardData)
        if (card.spec !== 'chara_card_v3') {
            alertError(language.errors.noData)
            return
        }
        let lorebook: loreBook[] = null
        if (importer.moduleData) {
            const md = await readModule(Buffer.from(importer.moduleData))
            card.data.extensions ??= {}
            card.data.extensions.risuai ??= {}
            card.data.extensions.risuai.triggerscript = md.trigger ?? []
            card.data.extensions.risuai.customScripts = md.regex ?? []
            if (md.lorebook) {
                lorebook = md.lorebook
            }
        }
        await importer.done()
        const v = await importCharacterCardSpec(
            card,
            undefined,
            'normal',
            importer.assets,
            lorebook,
            f.returnCharacter,
        )
        if (f.returnCharacter) {
            return v as any
        }
        return getDatabase().characters.findIndex(
            (character) => character.chaId === v,
        )
    }

    if (extension !== 'png') {
        alertError(language.errors.noData)
        return
    }

    alertStore.set({
        type: 'wait',
        msg: 'Loading... (Reading)',
    })
    await sleep(10)

    // const readed = PngChunk.read(img, ['chara'])?.['chara']
    let readedChara = ''
    let readedCCv3 = ''
    let img: Uint8Array
    let pngChunks = 0
    let readedPngChunks = 0

    {
        let readData: File | Uint8Array | ReadableStream<Uint8Array>
        if (f.data instanceof ReadableStream) {
            const tee = f.data.tee()
            f.data = tee[0]
            readData = tee[1]
        } else {
            readData = f.data
        }

        const prereader = PngChunk.readGenerator(readData, {})

        for await (const chunk of prereader) {
            if (chunk instanceof AppendableBuffer) {
                break
            }
            if (chunk.key.startsWith('chara-ext-asset_')) {
                pngChunks++
            }
        }
    }

    const readGenerator = PngChunk.readGenerator(f.data, {
        returnTrimed: true,
    })
    const assets: { [key: string]: string } = {}
    let queueFetch: Promise<Response>[] = []
    let queueFetchKey: string[] = []
    let queueFetchData: Buffer[] = []
    for await (const chunk of readGenerator) {
        if (!chunk) {
            continue
        }
        if (chunk instanceof AppendableBuffer) {
            img = chunk.buffer
            break
        }
        if (chunk.key === 'chara') {
            //For memory reason, limit to 5MB
            if (readedChara.length < 5 * 1024 * 1024) {
                readedChara = chunk.value
            }
            continue
        }
        if (chunk.key === 'ccv3') {
            if (readedCCv3.length < 5 * 1024 * 1024) {
                readedCCv3 = chunk.value
            }
            continue
        }
        if (chunk.key.startsWith('chara-ext-asset_')) {
            const assetIndex = chunk.key
                .replace('chara-ext-asset_:', '')
                .replace('chara-ext-asset_', '')
            const assetData = Buffer.from(chunk.value, 'base64')
            if (pngChunks === 0) {
                alertWait('Loading... (Loaded ' + readedPngChunks + ' Assets)')
            } else {
                alertStore.set({
                    type: 'progress',
                    msg: 'Loading... (Loading Assets)',
                    submsg: ((readedPngChunks / pngChunks) * 100).toFixed(2),
                })
            }

            readedPngChunks++

            if (db.account?.useSync && f.lightningRealmImport) {
                const id = await hasher(assetData)
                const xid = 'assets/' + id + '.png'
                queueFetchKey.push(assetIndex)
                queueFetchData.push(assetData)
                queueFetch.push(fetch(`${realmHubURL}/rs/` + xid))
                assets[assetIndex] = 'xid:' + xid
                if (queueFetch.length > 10) {
                    const res = await Promise.all(queueFetch)
                    for (let i = 0; i < res.length; i++) {
                        if (res[i].status !== 200) {
                            const assetId = await saveAsset(queueFetchData[i])
                            assets[queueFetchKey[i]] = assetId
                        } else {
                            assets[queueFetchKey[i]] = assets[
                                queueFetchKey[i]
                            ].replace('xid:', '')
                        }
                    }
                    queueFetch = []
                    queueFetchKey = []
                    queueFetchData = []
                }
                continue
            }

            const assetId = await saveAsset(assetData)
            assets[assetIndex] = assetId
        }
    }

    if (queueFetch.length > 0) {
        const res = await Promise.all(queueFetch)
        for (let i = 0; i < res.length; i++) {
            if (res[i].status !== 200) {
                const assetId = await saveAsset(queueFetchData[i])
                assets[queueFetchKey[i]] = assetId
            } else {
                assets[queueFetchKey[i]] = assets[queueFetchKey[i]].replace(
                    'xid:',
                    '',
                )
            }
        }
        queueFetch = []
        queueFetchKey = []
        queueFetchData = []
    }

    if (!readedChara && !readedCCv3) {
        alertError(language.errors.noData)
        return
    }

    if (readedCCv3) {
        readedChara = readedCCv3
    }

    if (!img) {
        alertError(language.errors.noData)
        return
    }

    if (readedChara.startsWith('rcc||')) {
        const parts = readedChara.split('||')
        const type = parts[1]
        if (type === 'rccv1') {
            if (parts.length !== 5) {
                alertError(language.errors.noData)
                return
            }
            const encrypted = Buffer.from(parts[2], 'base64')
            const hashed = await hasher(encrypted)
            if (hashed !== parts[3]) {
                alertError(language.errors.noData)
                return
            }
            const metaData: RccCardMetaData = JSON.parse(
                Buffer.from(parts[4], 'base64').toString('utf-8'),
            )
            if (metaData.usePassword) {
                const password = await alertInput(language.inputCardPassword)
                if (!password) {
                    return
                } else {
                    try {
                        const decrypted = await decryptBuffer(
                            encrypted,
                            password,
                        )
                        const charaData: CharacterCardV2Risu = JSON.parse(
                            Buffer.from(decrypted).toString('utf-8'),
                        )
                        const importedId = await importCharacterCardSpec(
                            charaData,
                            img,
                            'normal',
                            assets,
                        )
                        if (importedId) {
                            return getDatabase().characters.findIndex(
                                (character) => character.chaId === importedId,
                            )
                        } else {
                            throw new Error('Error while importing')
                        }
                    } catch (error) {
                        alertError(language.errors.wrongPassword)
                        return
                    }
                }
            } else {
                const decrypted = await decryptBuffer(encrypted, 'RISU_NONE')
                try {
                    const charaData: CharacterCardV2Risu = JSON.parse(
                        Buffer.from(decrypted).toString('utf-8'),
                    )
                    const importedId = await importCharacterCardSpec(
                        charaData,
                        img,
                        'normal',
                        assets,
                    )
                    if (importedId) {
                        return getDatabase().characters.findIndex(
                            (character) => character.chaId === importedId,
                        )
                    }
                } catch (error) {
                    alertError(language.errors.noData)
                    return
                }
            }
        }
    }
    const parsed = JSON.parse(
        Buffer.from(readedChara, 'base64').toString('utf-8'),
    )
    //fix readedChara version pointing number instead of string because of previous version
    if (
        typeof (parsed as CharacterCardV2Risu)?.data?.character_version ===
        'number'
    ) {
        ;(parsed as CharacterCardV2Risu).data.character_version = (
            parsed as CharacterCardV2Risu
        ).data.character_version.toString()
    }

    if (parsed.spec !== 'chara_card_v2' && parsed.spec !== 'chara_card_v3') {
        const charaData: OldTavernChar = JSON.parse(
            Buffer.from(readedChara, 'base64').toString('utf-8'),
        )
        const imgp = await saveAsset(img)
        const importedId = await commitDetachedCharacter(
            convertOffSpecCards(charaData, imgp),
            'import-character-card',
        )
        alertNormal(language.importedCharacter)
        return getDatabase().characters.findIndex(
            (character) => character.chaId === importedId,
        )
    }
    const importedId = await importCharacterCardSpec(
        parsed,
        img,
        'normal',
        assets,
    )
    return importedId
        ? getDatabase().characters.findIndex(
              (character) => character.chaId === importedId,
          )
        : null
}

export const getRealmInfo = async (realmPath:string) => {
    const url = new URL(location.href);
    url.searchParams.delete('realm');
    window.history.pushState(null, '', url.toString());

    const res = await fetch(`${realmHubURL}/hub/info/${realmPath}`)
    if(res.status !== 200){
        alertError(await res.text())
        return
    }
    showRealmInfoStore.set(await res.json())
}

export const showRealmInfoStore:Writable<null|hubType> = writable(null)

export async function characterURLImport() {
    const realmPath = new URLSearchParams(location.search).get('realm')
    try {
        if (realmPath) {
            getRealmInfo(realmPath)
        }
    } catch (error) {}

    const charPath = new URLSearchParams(location.search).get('charahub')
    try {
        if (charPath) {
            alertWait('Loading from Chub...')
            const url = new URL(location.href)
            url.searchParams.delete('charahub')
            window.history.pushState(null, '', url.toString())
            const chara = await fetch(
                'https://api.chub.ai/api/characters/download',
                {
                    method: 'POST',
                    body: JSON.stringify({
                        format: 'tavern',
                        fullPath: charPath,
                        version: 'main',
                    }),
                    headers: {
                        'content-type': 'application/json',
                    },
                },
            )
            const img = new Uint8Array(await chara.arrayBuffer())
            await importCharacterProcess({
                name: 'charahub.png',
                data: img,
            })
        }
    } catch (error) {
        alertError(language.errors.noData)
        return null
    }

    const hash = location.hash
    if (hash.startsWith('#import=')) {
        location.hash = ''
        const url = hash.replace('#import=', '')
        try {
            const res = await fetch(url, {
                method: 'GET',
            })
            const data = new Uint8Array(await res.arrayBuffer())
            await importFile(getFileName(res), data)
            checkCharOrder()
        } catch (error) {
            alertError(language.errors.noData)
            return null
        }
    }
    if (hash.startsWith('#import_module=')) {
        const data = hash.replace('#import_module=', '')
        const importData = JSON.parse(
            Buffer.from(decodeURIComponent(data), 'base64').toString('utf-8'),
        )
        importData.id = v4()

        if (importData.lowLevelAccess) {
            const conf = await alertConfirm(language.lowLevelAccessConfirm)
            if (!conf) {
                return false
            }
        }
        DBState.db.modules.push(importData)
        alertNormal(language.successImport)
        SettingsMenuIndex.set(14)
        settingsOpen.set(true)
        return
    }
    if (hash.startsWith('#import_preset=')) {
        const data = hash.replace('#import_preset=', '')
        const importData = Buffer.from(decodeURIComponent(data), 'base64')
        await importPreset({
            name: 'imported.risupreset',
            data: importData,
        })
        SettingsMenuIndex.set(1)
        settingsOpen.set(true)
        return
    }
    if (hash.startsWith('#share_character')) {
        const data = await fetch('/sw/share/character')
        if (data.status !== 200) {
            return
        }
        const charx = new Uint8Array(await data.arrayBuffer())
        await importCharacterProcess({
            name: 'shared.charx',
            data: charx,
        })
    }
    if (hash.startsWith('#share_module')) {
        const data = await fetch('/sw/share/module')
        if (data.status !== 200) {
            return
        }
        const module = new Uint8Array(await data.arrayBuffer())
        const md = await readModule(Buffer.from(module))
        md.id = v4()
        DBState.db.modules.push(md)
        alertNormal(language.successImport)
        SettingsMenuIndex.set(14)
        settingsOpen.set(true)
    }
    if (hash.startsWith('#share_preset')) {
        const data = await fetch('/sw/share/preset')
        if (data.status !== 200) {
            return
        }
        const preset = new Uint8Array(await data.arrayBuffer())
        await importPreset({
            name: 'shared.risup',
            data: preset,
        })
        SettingsMenuIndex.set(1)
        settingsOpen.set(true)
    }
    if ('launchQueue' in window) {
        const handleFiles = async (files: FileSystemFileHandle[]) => {
            for (const f of files) {
                const file = await f.getFile()
                const data = new Uint8Array(await file.arrayBuffer())
                await importFile(f.name, data)
            }
        }
        //@ts-expect-error launchQueue is File Handling API for PWA, not yet in TypeScript's Window interface
        window.launchQueue.setConsumer((launchParams) => {
            if (launchParams.files && launchParams.files.length) {
                const files = launchParams.files as FileSystemFileHandle[]
                handleFiles(files)
            }
        })
    }

    registerOpenedFileListeners(importFile, async (path) => {
        if ((isTauriDesktop || isTauriIOS) && /\.(risunest|risudat|bin)$/i.test(path)) {
            const { restoreBackupFromNativeSource } = await import('./storage/portableBackupFileRouteProduction.svelte')
            await restoreBackupFromNativeSource({type:'desktopPath',path})
            return true
        }
        if (!(isTauriDesktop || isTauriIOS) || !/\.(json|png|charx|jpe?g|risum)$/i.test(path))
            return false
        if (/\.risum$/i.test(path)) {
            const { importPreparedNativeModuleContent } = await import(
                './process/modules'
            )
            await importPreparedNativeModuleContent({
                source: { type: 'desktopPath', path },
                displayName: path.split(/[\\/]/).at(-1)!,
            })
        } else {
            await importDesktopNativeCharacterPath(path, {
                readDesktopPath: readFile,
                nativeEnabled: () => nativeCharacterContentImportEnabled,
                nativeImport: importPreparedNativeCharacterContent,
                legacyImport: async ({ name, data }) => {
                    await importFile(name, data)
                    return null
                },
            })
        }
        return true
    })

    async function importFile(name: string, data: Uint8Array) {
        if (
            name.endsWith('.charx') ||
            name.endsWith('.jpg') ||
            name.endsWith('.jpeg') ||
            name.endsWith('.png')
        ) {
            await importCharacterProcess({
                name: name,
                data: data,
            })
            return
        }
        if (name.endsWith('.risupreset') || name.endsWith('.risup')) {
            await importPreset({
                name: name,
                data: data,
            })
            SettingsMenuIndex.set(1)
            settingsOpen.set(true)
            alertNormal(language.successImport)
            return
        }
        if (name.endsWith('risum')) {
            const md = await readModule(Buffer.from(data))
            md.id = v4()
            DBState.db.modules.push(md)
            alertNormal(language.successImport)
            SettingsMenuIndex.set(14)
            settingsOpen.set(true)
            return
        }
    }

    function getFileName(res: Response): string {
        return (
            getFromContent(res.headers.get('content-disposition')) ||
            getFromURL(res.url)
        )

        function getFromContent(contentDisposition: string) {
            if (!contentDisposition) return null
            const pattern =
                /filename\*=UTF-8''([^"';\n]+)|filename[^;\n=]*=["']?([^"';\n]+)["']?/
            const matches = contentDisposition.match(pattern)
            if (matches) {
                if (matches[1]) {
                    return decodeURIComponent(matches[1])
                } else if (matches[2]) {
                    return matches[2]
                }
            }
            return null
        }

        function getFromURL(url: string): string {
            try {
                const path = new URL(url).pathname
                return path.substring(path.lastIndexOf('/') + 1)
            } catch {
                return ''
            }
        }
    }
}


function convertOffSpecCards(charaData:OldTavernChar|CharacterCardV2Risu, imgp:string|undefined = undefined):character{
    const data = charaData.spec_version === '2.0' ? charaData.data : charaData
    const charbook = charaData.spec_version === '2.0' ? charaData.data.character_book : null
    let lorebook:loreBook[] = []
    let loresettings:undefined|loreSettings = undefined
    let loreExt:undefined|any = undefined
    if(charbook){
        const a = convertCharbook({
            lorebook,
            charbook,
            loresettings,
            loreExt
        })

        lorebook = a.lorebook
        loresettings = a.loresettings
        loreExt = a.loreExt
    }

    return {
        name: data.name ?? 'unknown name',
        firstMessage: data.first_mes ?? 'unknown first message',
        desc:  data.description ?? '',
        notes: '',
        chats: [{
            message: [],
            note: '',
            name: 'Chat 1',
            localLore: [],
            id: v4()
        }],
        chatPage: 0,
        image: imgp,
        emotionImages: [],
        bias: [],
        globalLore: lorebook,
        viewScreen: 'none',
        chaId: uuidv4(),
        sdData: defaultSdDataFunc(),
        utilityBot: false,
        customscript: [],
        exampleMessage: data.mes_example,
        creatorNotes:'',
        systemPrompt: (charaData.spec_version === '2.0' ? charaData.data.system_prompt : '') ?? '',
        postHistoryInstructions: (charaData.spec_version === '2.0' ? charaData.data.post_history_instructions : '') ?? '',
        alternateGreetings:[],
        tags:[],
        creator:"",
        characterVersion: '',
        personality: data.personality ?? '',
        scenario:data.scenario ?? '',
        firstMsgIndex: -1,
        replaceGlobalNote: "",
        triggerscript: [],
        additionalText: '',
        loreExt: loreExt,
        loreSettings: loresettings,
        chatFolders: []
        
    }
}

export async function exportChar(charaID:number):Promise<string> {
    const db = getDatabase({snapshot: true})
    let char = safeStructuredClone(db.characters[charaID])

    if(char.type === 'group'){
        return ''
    }

    const option = await alertCardExport()
    if(option.type === ''){
        if(option.type2 === 'charx' || option.type2 === 'charxJpeg'){
            try {
                const appendedJpeg = option.type2 === 'charxJpeg'
                const nativeResult = await exportNativeCharacterCharxFromPicker({
                    characterId: char.chaId,
                    suggestedName: `${char.name || 'character'}.${appendedJpeg ? 'jpeg' : 'charx'}`,
                    ...(appendedJpeg ? { container: 'appended-charx-jpeg' as const } : {}),
                    projectCharacter: (leasedDetail) => {
                        const leasedCharacter = {
                            ...safeStructuredClone(leasedDetail),
                            chats: [],
                        } as character
                        const card = createBaseV3(leasedCharacter)
                        const module:RisuModule = {
                            name: `${leasedCharacter.name} Module`,
                            description: `Module for ${leasedCharacter.name}`,
                            id: v4(),
                            trigger: card.data.extensions.risuai.triggerscript ?? [],
                            regex: card.data.extensions.risuai.customScripts ?? [],
                            lorebook: leasedCharacter.globalLore ?? [],
                        }
                        delete card.data.extensions.risuai.triggerscript
                        delete card.data.extensions.risuai.customScripts
                        return {
                            card: card as unknown as Record<string, unknown>,
                            module: module as unknown as Record<string, unknown>,
                        }
                    },
                })
                if(nativeResult !== undefined){
                    if(nativeResult) alertNormal(language.successExport)
                    return ''
                }
            }
            catch(error){
                alertError(error)
                return ''
            }
        }
        if(option.type2 === 'json' || option.type2 === 'png'){
            try {
                const nativeResult = await exportNativeCharacterCardFromPicker({
                    characterId: char.chaId,
                    suggestedName: `${char.name || 'character'}.${option.type2}`,
                    format: option.type2 === 'json' ? 'json-card' : 'png-card',
                    projectCharacter: (leasedDetail) => createBaseV3({
                        ...safeStructuredClone(leasedDetail),
                        chats: [],
                    } as character) as unknown as Record<string, unknown>,
                })
                if(nativeResult !== undefined){
                    if(nativeResult) alertNormal(language.successExport)
                    return ''
                }
            }
            catch(error){
                alertError(error)
                return ''
            }
        }
        if(!char.image){
            const res = await fetch('/none.webp')
            const data = new Uint8Array(await res.arrayBuffer())
            char.image = await saveAsset(data)
        }
        await exportCharacterCard(char, option.type2 === 'json' ? 'json' : (option.type2 === 'charx' ? 'charx' : option.type2 === 'charxJpeg' ? 'charxJpeg' : 'png'), {spec: 'v3'})
    }
    else if(option.type === 'ccv2'){
        if(!char.image){
            const res = await fetch('/none.webp')
            const data = new Uint8Array(await res.arrayBuffer())
            char.image = await saveAsset(data)
        }
        exportCharacterCard(char,'png', {spec: 'v2'})
    }
    else if(option.type === 'realm'){
        ShowRealmFrameStore.set("character")
    }
    else{
        return option.type
    }
    return ''
}


export type PreparedNativeCharacterCardMetadata = CharacterCardV2Risu | CharacterCardV3 | OldTavernChar

export interface PreparedNativeCharacterCardAsset {
    token: string
    logicalId: string
}

export interface PreparedNativeCharacterCardModule {
    trigger?: readonly triggerscript[]
    regex?: readonly customscript[]
    lorebook?: readonly loreBook[]
}

export interface PreparedNativeCharacterCardInput {
    card: PreparedNativeCharacterCardMetadata
    assets: readonly PreparedNativeCharacterCardAsset[]
    portraitLogicalId?: string
    module?: PreparedNativeCharacterCardModule
}

export async function decodePreparedNativePngCharacterCard(
    metadata: PreparedNativePngCardMetadata,
): Promise<PreparedNativeCharacterCardMetadata | null> {
    return await decodePreparedNativePngCardMetadata(metadata, {
        hash: hasher,
        decrypt: async (bytes, password) => new Uint8Array(await decryptBuffer(bytes, password)),
        requestPassword: async () => (await alertInput(language.inputCardPassword)) || null,
    }) as PreparedNativeCharacterCardMetadata | null
}

interface CardAssetImportPolicy {
    initialImage?: string
    rejectPayload(reference: string): never
}

export async function mapPreparedNativeCharacterCard(input: PreparedNativeCharacterCardInput): Promise<character | false> {
    const assetDict = createPreparedNativeAssetDict(input.assets)
    if(input.portraitLogicalId !== undefined && input.portraitLogicalId.trim().length === 0){
        throw new Error('Prepared native portrait logical ID must not be empty')
    }

    if (!isPreparedNativeSpecificationCard(input.card)) {
        return convertOffSpecCards(input.card, input.portraitLogicalId)
    }

    preflightPreparedNativeCard(input.card, assetDict, input.portraitLogicalId)

    const card = safeStructuredClone(input.card)
    let overrideLorebook:loreBook[] = null
    if(input.module){
        const data = card.data as typeof card.data & {
            extensions: Record<string, any>
        }
        data.extensions ??= {}
        data.extensions.risuai ??= {}
        data.extensions.risuai.triggerscript = [...safeStructuredClone(input.module.trigger ?? [])]
        data.extensions.risuai.customScripts = [...safeStructuredClone(input.module.regex ?? [])]
        if(input.module.lorebook !== undefined){
            overrideLorebook = [...safeStructuredClone(input.module.lorebook)]
        }
    }

    return importCharacterCardSpecWithPolicy(
        card,
        undefined,
        'normal',
        assetDict,
        overrideLorebook,
        true,
        {
            initialImage: input.portraitLogicalId,
            rejectPayload(reference): never {
                throw new Error(`Prepared native card cannot load payload reference: ${reference}`)
            },
        },
    )
}

function isPreparedNativeSpecificationCard(
    card: PreparedNativeCharacterCardMetadata,
): card is CharacterCardV2Risu | CharacterCardV3 {
    return 'spec' in card
        && (card.spec === 'chara_card_v2' || card.spec === 'chara_card_v3')
}

function createPreparedNativeAssetDict(assets: readonly PreparedNativeCharacterCardAsset[]): Record<string, string> {
    const assetDict:Record<string, string> = Object.create(null)
    for(const asset of assets){
        if(asset.token.trim().length === 0 || asset.logicalId.trim().length === 0){
            throw new Error('Prepared native asset token and logical ID must not be empty')
        }
        if(Object.hasOwn(assetDict, asset.token)){
            throw new Error(`Duplicate prepared native asset token: ${asset.token}`)
        }
        assetDict[asset.token] = asset.logicalId
    }
    return assetDict
}

function preflightPreparedNativeCard(card: CharacterCardV2Risu | CharacterCardV3, assetDict: Record<string, string>, portraitLogicalId?: string): void {
    if(card.spec === 'chara_card_v3'){
        for(const asset of card.data.assets ?? []){
            if(asset.uri.startsWith('data:')){
                throw new Error('Prepared native card data URI payloads are not allowed')
            }
            if(asset.uri.startsWith('__asset:')){
                requirePreparedNativeAsset(asset.uri.slice('__asset:'.length), assetDict)
            }
            else if(asset.uri.startsWith('embeded://')){
                requirePreparedNativeAsset(asset.uri.slice('embeded://'.length), assetDict)
            }
            else if(asset.uri === 'ccdefault:' && !portraitLogicalId){
                throw new Error('Prepared native ccdefault asset requires a portrait logical ID')
            }
        }
        return
    }

    const risuai = card.data.extensions.risuai
    for(const emotion of risuai?.emotions ?? []){
        requirePreparedNativeV2Reference(emotion[1], assetDict)
    }
    for(const asset of risuai?.additionalAssets ?? []){
        requirePreparedNativeV2Reference(asset[1], assetDict)
    }
    for(const reference of Object.values(risuai?.vits ?? {})){
        requirePreparedNativeV2Reference(reference, assetDict)
    }
}

function requirePreparedNativeV2Reference(reference: string, assetDict: Record<string, string>): void {
    if(!reference.startsWith('__asset:')){
        throw new Error('Prepared native v2 payload fields require a logical asset reference')
    }
    requirePreparedNativeAsset(reference.slice('__asset:'.length), assetDict)
}

function requirePreparedNativeAsset(token: string, assetDict: Record<string, string>): void {
    if(token.length === 0 || !Object.hasOwn(assetDict, token)){
        throw new Error(`Prepared native asset token is missing: ${token}`)
    }
}

export async function importCharacterCardSpec<T extends boolean = false>(card:CharacterCardV2Risu|CharacterCardV3, img?:Uint8Array, mode:'hub'|'normal' = 'normal', assetDict:{[key:string]:string} = {}, overrideLorebook: loreBook[] = null, returnValue:T = false as T):Promise<T extends true ? character|false : string|false>{
    return importCharacterCardSpecWithPolicy(card, img, mode, assetDict, overrideLorebook, returnValue)
}

async function importCharacterCardSpecWithPolicy<T extends boolean = false>(
    card: CharacterCardV2Risu | CharacterCardV3,
    img?: Uint8Array,
    mode: 'hub' | 'normal' = 'normal',
    assetDict: { [key: string]: string } = {},
    overrideLorebook: loreBook[] = null,
    returnValue: T = false as T,
    assetPolicy?: CardAssetImportPolicy,
): Promise<T extends true ? character | false : string | false> {
    if (
        !card ||
        (card.spec !== 'chara_card_v2' && card.spec !== 'chara_card_v3')
    ) {
        return false
    }

    console.log(`Importing ${card.spec}, mode is ${mode}`)

    let lastProgressAt = 0
    async function updateAssetProgress(
        index: number,
        total: number,
        label: string,
    ) {
        const now = performance.now()
        if (now - lastProgressAt < 50) return
        lastProgressAt = now
        if (!assetPolicy)
            alertStore.set({
                type: 'progress',
                msg: label,
                submsg: ((index / total) * 100).toFixed(2),
            })
        await sleep(0)
    }
    const data = card.data
    let im = assetPolicy
        ? assetPolicy.initialImage
        : img
          ? await saveAsset(img)
          : undefined
    let db = DBState.db

    const risuext = safeStructuredClone(data.extensions.risuai)
    let emotions: [string, string][] = []
    let bias: [string, number][] = []
    let viewScreen: 'none' | 'emotion' | 'imggen' = 'none'
    let customScripts: customscript[] = []
    let utilityBot = false
    let sdData = defaultSdDataFunc()
    let extAssets: [string, string, string][] = []
    let ccAssets: {
        type: string
        uri: string
        name: string
        ext: string
    }[] = []

    let vits: null | OnnxModelFiles = null
    if (risuext && card.spec === 'chara_card_v2') {
        if (risuext.emotions) {
            for (let i = 0; i < risuext.emotions.length; i++) {
                await updateAssetProgress(
                    i,
                    risuext.emotions.length,
                    `Loading... (Loading Emotions)`,
                )
                if (risuext.emotions[i][1].startsWith('__asset:')) {
                    const key = risuext.emotions[i][1].replace('__asset:', '')
                    const imgp = assetDict[key]
                    if (!imgp) {
                        throw new Error(
                            'Error while importing, asset ' +
                                key +
                                ' not found',
                        )
                    }
                    emotions.push([risuext.emotions[i][0], imgp])
                    continue
                }
                assetPolicy?.rejectPayload(risuext.emotions[i][1])
                const imgp = await saveAsset(
                    mode === 'hub'
                        ? await getHubResources(risuext.emotions[i][1])
                        : Buffer.from(risuext.emotions[i][1], 'base64'),
                )
                emotions.push([risuext.emotions[i][0], imgp])
            }
        }
        if (risuext.additionalAssets) {
            for (let i = 0; i < risuext.additionalAssets.length; i++) {
                await updateAssetProgress(
                    i,
                    risuext.additionalAssets.length,
                    `Loading... (Loading Assets)`,
                )
                let fileName = ''
                if (risuext.additionalAssets[i].length >= 3)
                    fileName = risuext.additionalAssets[i][2]
                if (risuext.additionalAssets[i][1].startsWith('__asset:')) {
                    const key = risuext.additionalAssets[i][1].replace(
                        '__asset:',
                        '',
                    )
                    const imgp = assetDict[key]
                    if (!imgp) {
                        throw new Error(
                            'Error while importing, asset ' +
                                key +
                                ' not found',
                        )
                    }
                    extAssets.push([
                        risuext.additionalAssets[i][0],
                        imgp,
                        fileName,
                    ])
                    continue
                }
                assetPolicy?.rejectPayload(risuext.additionalAssets[i][1])
                const imgp = await saveAsset(
                    mode === 'hub'
                        ? await getHubResources(risuext.additionalAssets[i][1])
                        : Buffer.from(risuext.additionalAssets[i][1], 'base64'),
                    '',
                    fileName,
                )
                extAssets.push([risuext.additionalAssets[i][0], imgp, fileName])
            }
        }
        if (risuext.vits) {
            const keys = Object.keys(risuext.vits)
            for (let i = 0; i < keys.length; i++) {
                await updateAssetProgress(
                    i,
                    keys.length,
                    `Loading... (Loading VITS)`,
                )
                const key = keys[i]
                if (risuext.vits[key].startsWith('__asset:')) {
                    const rkey = risuext.vits[key].replace('__asset:', '')
                    const imgp = assetDict[rkey]
                    if (!imgp) {
                        throw new Error(
                            'Error while importing, asset ' +
                                rkey +
                                ' not found',
                        )
                    }
                    risuext.vits[key] = imgp
                    continue
                }
                assetPolicy?.rejectPayload(risuext.vits[key])
                const imgp = await saveAsset(
                    mode === 'hub'
                        ? await getHubResources(risuext.vits[key])
                        : Buffer.from(risuext.vits[key], 'base64'),
                )
                risuext.vits[key] = imgp
            }

            if (keys.length > 0) {
                vits = {
                    name: 'Imported VITS',
                    files: risuext.vits,
                    id: uuidv4().replace(/-/g, ''),
                }
            }
        }

        if (risuext) {
            bias = risuext.bias ?? bias
            viewScreen = risuext.viewScreen ?? viewScreen
            customScripts = risuext.customScripts ?? customScripts
            utilityBot = risuext.utilityBot ?? utilityBot
            sdData = risuext.sdData ?? sdData
        }
    }
    if (card.spec === 'chara_card_v3') {
        const data = card.data //required for type checking
        if (data.assets) {
            for (let i = 0; i < data.assets.length; i++) {
                await updateAssetProgress(
                    i,
                    data.assets.length,
                    `Loading... (Assets)`,
                )
                let fileName = ''
                let imgp = ''
                if (data.assets[i].name) {
                    fileName = data.assets[i].name
                }
                if (data.assets[i].uri.startsWith('__asset:')) {
                    const key = data.assets[i].uri.replace('__asset:', '')
                    imgp = assetDict[key]
                    if (!imgp) {
                        throw new Error(
                            'Error while importing, asset ' +
                                key +
                                ' not found',
                        )
                    }
                } else if (data.assets[i].uri === 'ccdefault:') {
                    imgp = im
                } else if (data.assets[i].uri.startsWith('embeded://')) {
                    const key = data.assets[i].uri.replace('embeded://', '')
                    imgp = assetDict[key]
                    if (!imgp) {
                        throw new Error(
                            'Error while importing, asset ' +
                                key +
                                ' not found',
                        )
                    }
                } else if (data.assets[i].uri.startsWith('data:')) {
                    assetPolicy?.rejectPayload(data.assets[i].uri)
                    //data uri
                    const b64 = data.assets[i].uri.split(',')[1]
                    if (b64.length < 50 * 1024 * 1024) {
                        imgp = await saveAsset(Buffer.from(b64, 'base64'))
                    } else {
                        alertError('Data URI too large')
                        continue
                    }
                } else {
                    continue
                }
                if (data.assets[i].type === 'emotion') {
                    emotions.push([fileName, imgp])
                } else if (data.assets[i].type === 'x-risu-asset') {
                    extAssets.push([
                        fileName,
                        imgp,
                        data.assets[i].ext ?? 'unknown',
                    ])
                } else if (
                    data.assets[i].type === 'icon' &&
                    data.assets[i].name === 'main'
                ) {
                    im = imgp
                } else {
                    ccAssets.push({
                        type: data.assets[i].type ?? 'asset',
                        uri: imgp,
                        name: fileName,
                        ext: data.assets[i].ext ?? 'unknown',
                    })
                }
            }
        }

        if (risuext) {
            bias = risuext.bias ?? bias
            viewScreen = risuext.viewScreen ?? viewScreen
            customScripts = risuext.customScripts ?? customScripts
            utilityBot = risuext.utilityBot ?? utilityBot
            sdData = risuext.sdData ?? sdData
        }
    }

    if (risuext && risuext?.lowLevelAccess) {
        const conf = await alertConfirm(language.lowLevelAccessConfirm)
        if (!conf) {
            return false
        }
    }
    const charbook = data.character_book
    let lorebook: loreBook[] = overrideLorebook ?? []
    let loresettings: undefined | loreSettings = undefined
    let loreExt: undefined | any = undefined
    if (charbook) {
        const a = convertCharbook({
            lorebook: overrideLorebook ? [] : lorebook,
            charbook,
            loresettings,
            loreExt,
        })

        if (!overrideLorebook) {
            lorebook = a.lorebook
        }
        loresettings = a.loresettings
        loreExt = a.loreExt
    }

    let ext = safeStructuredClone(data?.extensions ?? {})

    for (const key in ext) {
        if (key === 'risuai') {
            delete ext[key]
        }
        if (key === 'depth_prompt') {
            delete ext[key]
        }
    }

    let char: character = {
        name: data.name ?? '',
        firstMessage: data.first_mes ?? '',
        desc: data.description ?? '',
        notes: '',
        chats: [
            {
                message: [],
                note: '',
                name: 'Chat 1',
                localLore: [],
                id: v4(),
            },
        ],
        chatPage: 0,
        image: im,
        emotionImages: emotions,
        bias: bias,
        globalLore: lorebook, //lorebook
        viewScreen: viewScreen,
        chaId: uuidv4(),
        sdData: sdData,
        utilityBot: utilityBot,
        customscript: customScripts,
        exampleMessage: data.mes_example ?? '',
        creatorNotes: data.creator_notes ?? '',
        systemPrompt: data.system_prompt ?? '',
        postHistoryInstructions: '',
        alternateGreetings: data.alternate_greetings ?? [],
        tags: data.tags ?? [],
        creator: data.creator ?? '',
        characterVersion: `${data.character_version}` || '',
        personality: data.personality ?? '',
        scenario: data.scenario ?? '',
        firstMsgIndex: -1,
        removedQuotes: false,
        loreSettings: loresettings,
        loreExt: loreExt,
        additionalData: {
            tag: data.tags ?? [],
            creator: data.creator,
            character_version: data.character_version,
        },
        additionalAssets: extAssets,
        replaceGlobalNote: data.post_history_instructions ?? '',
        backgroundHTML: data?.extensions?.risuai?.backgroundHTML,
        license: data?.extensions?.risuai?.license,
        triggerscript: data?.extensions?.risuai?.triggerscript ?? [],
        private: data?.extensions?.risuai?.private ?? false,
        additionalText: data?.extensions?.risuai?.additionalText ?? '',
        virtualscript: '', //removed dude to security issue
        extentions: ext ?? {},
        largePortrait:
            data?.extensions?.risuai?.largePortrait ??
            !data?.extensions?.risuai,
        lorePlus: data?.extensions?.risuai?.lorePlus ?? false,
        inlayViewScreen: data?.extensions?.risuai?.inlayViewScreen ?? false,
        newGenData: data?.extensions?.risuai?.newGenData ?? undefined,
        vits: vits,
        ttsMode: vits ? 'vits' : 'normal',
        imported: true,
        source: card?.data?.extensions?.risuai?.source ?? [],
        ccAssets: ccAssets,
        lowLevelAccess: risuext?.lowLevelAccess ?? false,
        defaultVariables: data?.extensions?.risuai?.defaultVariables ?? '',
        chatFolders: [],
        prebuiltAssetCommand:
            data?.extensions?.risuai?.prebuiltAssetCommand ?? '',
        prebuiltAssetExclude:
            data?.extensions?.risuai?.prebuiltAssetExclude ?? [],
        prebuiltAssetStyle: data?.extensions?.risuai?.prebuiltAssetStyle ?? '',
        customModuleToggle: data?.extensions?.risuai?.toggles ?? {},
        moduleNamespace: data?.extensions?.risuai?.moduleNamespace,
        hideChatIcon: data?.extensions?.risuai?.hideChatIcon ?? false,
    }

    if (card.spec === 'chara_card_v3') {
        char.group_only_greetings = card.data.group_only_greetings ?? []
        char.nickname = card.data.nickname ?? ''
        char.source =
            card.data.source ?? card.data?.extensions?.risuai?.source ?? []
        char.creation_date = card.data.creation_date ?? 0
        char.modification_date = card.data.modification_date ?? 0
    }

    if (returnValue) {
        return char as any
    }

    await commitDetachedCharacter(char, 'import-character-card')
    alertNormal(language.importedCharacter)
    return char.chaId as any
}

function convertCharbook(arg:{
    lorebook:loreBook[]
    charbook:CharacterBook
    loresettings:loreSettings
    loreExt:any
}){
    let {lorebook, loresettings, loreExt, charbook} = arg
    if((!checkNullish(charbook.recursive_scanning)) &&
        (!checkNullish(charbook.scan_depth)) &&
        (!checkNullish(charbook.token_budget))){
        loresettings = {
            tokenBudget:charbook.token_budget,
            scanDepth:charbook.scan_depth,
            recursiveScanning: charbook.recursive_scanning,
            fullWordMatching: charbook?.extensions?.risu_fullWordMatching ?? false,
        }
    }

    loreExt = charbook.extensions

    for(const book of charbook.entries){
        let content = book.content

        if(book.use_regex && !book.keys?.[0]?.startsWith('/')){
            book.use_regex = false
        }

        //extention migration
        const extensions = book.extensions ?? {}

        if(extensions.useProbability && extensions.probability !== undefined && extensions.probability !== 100){
            content = `@@probability ${extensions.probability}\n` + content
            delete extensions.useProbability
            delete extensions.probability
        }
        if(extensions.position === 4 && typeof extensions.depth === 'number' && typeof(extensions.role) === 'number'){
            content = `@@depth ${extensions.depth}\n@@role ${['system','user','assistant'][extensions.role]}\n` + content
            delete extensions.position
            delete extensions.depth
            delete extensions.role
        }
        if(typeof(extensions.selectiveLogic) === 'number' && book.secondary_keys && book.secondary_keys.length > 0){
            switch(extensions.selectiveLogic){
                case 0:{
                    if(!book.secondary_keys || book.secondary_keys.length === 0){
                        book.selective = false
                    }
                    break
                }
                case 1:{
                    book.selective = false
                    content = `@@exclude_keys_all ${book.secondary_keys.join(',')}\n` + content
                    break
                }
                case 2:{
                    book.selective = false
                    for(const secKey of book.secondary_keys){
                        content = `@@exclude_keys ${secKey}\n` + content
                    }
                    break
                }
                case 3:{
                    book.selective = false
                    for(const secKey of book.secondary_keys){
                        content = `@@additional_keys ${secKey}\n` + content
                    }
                    break
                }
            }
        }
        if(typeof extensions.delay === 'number' && extensions.delay > 0){
            content = `@@activate_only_after ${extensions.delay}\n` + content
            delete extensions.delay
        }
        if(extensions.match_whole_words === true){
            content = `@@match_full_word\n` + content
            delete extensions.match_whole_words
        }
        if(extensions.match_whole_words === false){
            content = `@@match_partial_word\n` + content
            delete extensions.match_whole_words
        }

        lorebook.push({
            key: book.keys.join(', '),
            secondkey: book.secondary_keys?.join(', ') ?? '',
            insertorder: book.insertion_order,
            comment: book.name ?? book.comment ?? "",
            content: content,
            mode: (book.mode as any) ?? "normal",
            alwaysActive: book.constant ?? false,
            selective: book.selective ?? false,
            extentions: {...extensions, risu_case_sensitive: book.case_sensitive},
            activationPercent: book.extensions?.risu_activationPercent,
            loreCache: book.extensions?.risu_loreCache ?? null,
            useRegex: book.use_regex ?? false,
            folder: book.folder
        })
    }

    return {
        lorebook,
        loresettings,
        loreExt
    }
}



function createBaseV2(char:character) {
    
    let charBook:charBookEntry[] = []
    for(const lore of char.globalLore){
        let ext:{
            risu_case_sensitive?: boolean;
            risu_activationPercent?: number
            risu_loreCache?: {
                key:string
                data:string[]
            }
        } = safeStructuredClone(lore.extentions ?? {})

        let caseSensitive = ext.risu_case_sensitive ?? false
        ext.risu_activationPercent = lore.activationPercent
        ext.risu_loreCache = lore.loreCache

        charBook.push({
            keys: lore.key.split(',').map(r => r.trim()),
            secondary_keys: lore.selective ? lore.secondkey.split(',').map(r => r.trim()) : undefined,
            content: lore.content,
            extensions: ext,
            enabled: true,
            insertion_order: lore.insertorder,
            constant: lore.alwaysActive,
            selective:lore.selective,
            name: lore.comment,
            comment: lore.comment,
            case_sensitive: caseSensitive,
            mode: lore.mode ?? "normal",
            folder: lore.folder,
        })
    }
    char.loreExt ??= {}

    char.loreExt.risu_fullWordMatching = char.loreSettings?.fullWordMatching ?? false

    const card:CharacterCardV2Risu = {
        spec: "chara_card_v2",
        spec_version: "2.0",
        data: {
            name: char.name,
            description: char.desc ?? '',
            personality: char.personality ?? '',
            scenario: char.scenario ?? '',
            first_mes: char.firstMessage ?? '',
            mes_example: char.exampleMessage ?? '',
            creator_notes: char.creatorNotes ?? '',
            system_prompt: char.systemPrompt ?? '',
            post_history_instructions: char.replaceGlobalNote ?? '',
            alternate_greetings: char.alternateGreetings ?? [],
            character_book: {
                scan_depth: char.loreSettings?.scanDepth,
                token_budget: char.loreSettings?.tokenBudget,
                recursive_scanning: char.loreSettings?.recursiveScanning,
                extensions: char.loreExt ?? {},
                entries: charBook
            },
            tags: char.tags ?? [],
            creator: char.additionalData?.creator ?? '',
            character_version: `${char.additionalData?.character_version}` || '',
            extensions: {
                risuai: {
                    // emotions: char.emotionImages,
                    bias: char.bias,
                    viewScreen: char.viewScreen,
                    customScripts: char.customscript,
                    utilityBot: char.utilityBot,
                    sdData: char.sdData,
                    // additionalAssets: char.additionalAssets,
                    backgroundHTML: char.backgroundHTML,
                    license: char.license,
                    triggerscript: char.triggerscript,
                    additionalText: char.additionalText,
                    virtualscript: '', //removed dude to security issue
                    largePortrait: char.largePortrait,
                    lorePlus: char.lorePlus,
                    inlayViewScreen: char.inlayViewScreen,
                    newGenData: char.newGenData,
                    vits: {}
                },
                depth_prompt: char.depth_prompt
            }
        }
    }

    if(char.extentions){
        for(const key in char.extentions){
            if(key === 'risuai' || key === 'depth_prompt'){
                continue
            }
            card.data.extensions[key] = char.extentions[key]
        }
    }
    return card
}


export async function exportCharacterCard(char:character, type:'png'|'json'|'charx'|'charxJpeg' = 'png', arg:{
    password?:string
    writer?:LocalWriter|VirtualWriter,
    spec?:'v2'|'v3'
} = {}) {
    let img = await readImage(char.image)
    const spec:'v2'|'v3' = arg.spec ?? 'v2' //backward compatibility
    try{
        char.image = ''
        img = type === 'png' ? (await reencodeImage(img)) : img
        const localWriter = arg.writer ?? (new LocalWriter())
        if(!arg.writer && type !== 'json'){
            const nameExt = {
                'png': ['Image File', 'png'],
                'json': ['JSON File', 'json'],
                'charx': ['CharX File', 'charx'],
                'charxJpeg': ['CharX Embeded Jpeg', 'jpeg']
            }
            const ext = nameExt[type]
            await (localWriter as LocalWriter).init(ext[0], [ext[1]], `${char.name || 'character'}.${ext[1]}`)
        }
        const writer = (type === 'charx' || type === 'charxJpeg') ? (new CharXWriter(localWriter)) : type === 'json' ? (new BlankWriter()) : (new PngChunk.streamWriter(img, localWriter))
        await writer.init()
        if(writer instanceof CharXWriter && type === 'charxJpeg'){
            await writer.writeJpeg(img)
        }
        let assetIndex = 0
        if(spec === 'v2'){
            const card = await createBaseV2(char)
            if(card.data.extensions.risuai.emotions && card.data.extensions.risuai.emotions.length > 0){
                for(let i=0;i<card.data.extensions.risuai.emotions.length;i++){
                    alertStore.set({
                        type: 'progress',
                        msg: 'Loading... (Adding Emotions)',
                        submsg: (i / card.data.extensions.risuai.emotions.length * 100).toFixed(2)
                    })
                    const key = card.data.extensions.risuai.emotions[i][1]
                    const rData = await readImage(key)
                    const b64encoded = Buffer.from(rData).toString('base64')
                    assetIndex++
                    card.data.extensions.risuai.emotions[i][1] = `__asset:${assetIndex}`
                    await writer.write("chara-ext-asset_:" + assetIndex, b64encoded)
                }
            }
    
            
            if(card.data.extensions.risuai.additionalAssets && card.data.extensions.risuai.additionalAssets.length > 0){
                for(let i=0;i<card.data.extensions.risuai.additionalAssets.length;i++){
                    alertStore.set({
                        type: 'progress',
                        msg: 'Loading... (Adding Additional Assets)',
                        submsg: (i / card.data.extensions.risuai.additionalAssets.length * 100).toFixed(2)
                    })
                    const key = card.data.extensions.risuai.additionalAssets[i][1]
                    const rData = await readImage(key)
                    const b64encoded = Buffer.from(rData).toString('base64')
                    assetIndex++
                    card.data.extensions.risuai.additionalAssets[i][1] = `__asset:${assetIndex}`
                    await writer.write("chara-ext-asset_:" + assetIndex, b64encoded)
                }
            }
    
            if(char.vits && char.ttsMode === 'vits'){
                const keys = Object.keys(char.vits.files)
                for(let i=0;i<keys.length;i++){
                    alertStore.set({
                        type: 'progress',
                        msg: 'Loading... (Adding VITS)',
                        submsg: (i / keys.length * 100).toFixed(2)
                    })
                    const key = keys[i]
                    const rData = await loadAsset(char.vits.files[key])
                    const b64encoded = Buffer.from(rData).toString('base64')
                    assetIndex++
                    card.data.extensions.risuai.vits[key] = `__asset:${assetIndex}`
                    await writer.write("chara-ext-asset_:" + assetIndex, b64encoded)
                }
            }
            if(type === 'json'){
                await downloadFile(`${char.name.replace(/[<>:"/\\|?*\.\,]/g, "")}_export.json`, Buffer.from(JSON.stringify(card, null, 4), 'utf-8'))
                alertNormal(language.successExport)
                return
            }
    
            await sleep(10)
            alertStore.set({
                type: 'wait',
                msg: 'Loading... (Writing)'
            })
    
            await writer.write("chara", Buffer.from(JSON.stringify(card)).toString('base64'))     
        }
        else if(spec === 'v3'){
            const card = createBaseV3(char)
            const seenPaths = new Set<string>()
            if(card.data.assets && card.data.assets.length > 0){
                for(let i=0;i<card.data.assets.length;i++){
                    alertStore.set({
                        type: 'progress',
                        msg: 'Loading... (Adding Assets)',
                        submsg: (i / card.data.assets.length * 100).toFixed(2)
                    })
                    let key = card.data.assets[i].uri
                    let rData:Uint8Array
                    if(key === 'ccdefault:' && type !== 'png'){
                        key = char.image
                        rData = img
                    }
                    else if(isKnownUri(key)){
                        continue
                    }
                    else{
                        rData = await readImage(key)
                    }
                    assetIndex++
                    if(type === 'png'){
                        const b64encoded = Buffer.from(rData).toString('base64')
                        card.data.assets[i].uri = `__asset:${assetIndex}`
                        await writer.write("chara-ext-asset_:" + assetIndex, b64encoded)
                    }
                    else if(type === 'json'){
                        const b64encoded = Buffer.from(rData).toString('base64')
                        card.data.assets[i].uri = `data:application/octet-stream;base64,${b64encoded}`
                    }
                    else{
                        let type = 'other'
                        let itype = 'other'
                        switch(card.data.assets[i].type){
                            case 'emotion':
                                type = 'emotion'
                                break
                            case 'background':
                                type = 'background'
                                break
                            case 'user_icon':
                                type = 'user_icon'
                                break
                            case 'icon':
                                type = 'icon'
                                break
                        }
                        switch(card.data.assets[i].ext){
                            case 'png':
                            case 'jpg':
                            case 'jpeg':
                            case 'gif':
                            case 'webp':
                            case 'avif':
                                itype = 'image'
                                break
                            case 'mp3':
                            case 'wav':
                            case 'ogg':
                            case 'flac':
                                itype = 'audio'
                                break
                            case 'mp4':
                            case 'webm':
                            case 'mov':
                            case 'avi':
                            case 'mkv':
                                itype = 'video'
                                break
                            case 'mmd':
                            case 'obj':
                                itype = 'model'
                                break
                            case 'safetensors':
                            case 'cpkt':
                            case 'onnx':
                                itype = 'ai'
                                break
                            case 'otf':
                            case 'ttf':
                            case 'woff':
                            case 'woff2':
                                itype = 'fonts'
                                break
                            case 'js':
                            case 'ts':
                            case 'lua':
                                itype = 'code'
                        }

                        let path = ''
                        let name = card.data.assets[i].name || `asset_${assetIndex}`
                        if(name.length > 100){
                            name = name.substring(0,100)
                        }
                        const ext = card.data.assets[i].ext === 'unknown' ? 'png' : card.data.assets[i].ext
                        const baseDir = card.data.assets[i].ext === 'unknown'
                            ? `assets/${type}/image`
                            : `assets/${type}/${itype}`

                        // Generate unique path to avoid duplicate filenames
                        let uniqueName = name
                        let suffix = 0
                        while(seenPaths.has(`${baseDir}/${uniqueName}.${ext}`)){
                            suffix++
                            uniqueName = `${name}_${suffix}`
                        }
                        path = `${baseDir}/${uniqueName}.${ext}`
                        seenPaths.add(path)

                        card.data.assets[i].uri = 'embeded://' + path
                        const imageType = getImageType(rData)
                        const metaPath = `x_meta/${uniqueName}.json`
                        if(imageType === 'PNG' && writer instanceof CharXWriter){
                            const metadatas:Record<string,string> = {}
                            const gen = PngChunk.readGenerator(rData)
                            for await (const chunk of gen){
                                if(!chunk || chunk instanceof AppendableBuffer){
                                    continue
                                }
                                metadatas[chunk.key] = chunk.value
                            }
                            if(Object.keys(metadatas).length > 0){
                                await writer.write(metaPath, Buffer.from(JSON.stringify(metadatas, null, 4)), 6)
                            }
                            else{
                                await writer.write(metaPath, Buffer.from(JSON.stringify({
                                    'type': imageType
                                }), 'utf-8'), 6)
                            }
                        }
                        else{
                            await writer.write(metaPath, Buffer.from(JSON.stringify({
                                'type': imageType
                            }), 'utf-8'), 6)
                        }
                        await writer.write(path, Buffer.from(rData))
                    }
                }
            }
            if(type === 'json'){
                await downloadFile(`${char.name.replace(/[<>:"/\\|?*\.\,]/g, "")}_export.json`, Buffer.from(JSON.stringify(card, null, 4), 'utf-8'))
                alertNormal(language.successExport)
                return
            }

            await sleep(10)
            alertStore.set({
                type: 'wait',
                msg: 'Loading... (Writing)'
            })
    
            if(type === 'charx' || type === 'charxJpeg'){
                const md:RisuModule = {
                    name: `${char.name} Module`,
                    description: "Module for " + char.name,
                    id: v4(),
                    trigger: card.data.extensions.risuai.triggerscript ?? [],
                    regex: card.data.extensions.risuai.customScripts ?? [],
                    lorebook: char.globalLore ?? [],
                }
                delete card.data.extensions.risuai.triggerscript
                delete card.data.extensions.risuai.customScripts
                await writer.write("module.risum", await exportModuleLegacy(md, {
                    alertEnd: false,
                    saveData: false
                }))
                await writer.write("card.json", Buffer.from(JSON.stringify(card, null, 4)))
            }
            else{
                await writer.write("ccv3", Buffer.from(JSON.stringify(card)).toString('base64'))
            }
        }
        await writer.end()

        await sleep(10)

        if(!arg.writer){
            alertNormal(language.successExport)
        }

    }
    catch(e){
        alertError(e)
    }
}

// Extended LorebookEntry with Risuai specific fields
type RisuLorebookEntry = LorebookEntry & {
    mode?: string;
    folder?: string;
}

export function createBaseV3(char:character){
    
    let charBook:RisuLorebookEntry[] = []
    let assets:Array<{
        type: string
        uri: string
        name: string
        ext: string
    }> = safeStructuredClone(char.ccAssets ?? [])

    if(char.additionalAssets){
        for(const asset of char.additionalAssets){
            assets.push({
                type: 'x-risu-asset',
                uri: asset[1],
                name: asset[0],
                ext: asset[2] || 'png'
            })
        }
    }

    if(char.emotionImages){
        for(const asset of char.emotionImages){
            assets.push({
                type: 'emotion',
                uri: asset[1],
                name: asset[0],
                ext: 'png'
            })
        }
    
        assets.push({
            type: 'icon',
            uri: 'ccdefault:',
            name: 'main',
            ext: 'png'
        })
    }

    for(const lore of char.globalLore){
        let ext:{
            risu_case_sensitive?: boolean;
            risu_activationPercent?: number
            risu_loreCache?: {
                key:string
                data:string[]
            }
        } = safeStructuredClone(lore.extentions ?? {})

        let caseSensitive = ext.risu_case_sensitive ?? false
        ext.risu_activationPercent = lore.activationPercent
        ext.risu_loreCache = lore.loreCache

        charBook.push({
            ...{
                keys: lore.key.split(',').map(r => r.trim()),
                secondary_keys: lore.selective ? lore.secondkey.split(',').map(r => r.trim()) : undefined,
                content: lore.content,
                extensions: ext,
                enabled: true,
                insertion_order: lore.insertorder,
                constant: lore.alwaysActive,
                selective:lore.selective,
                name: lore.comment,
                comment: lore.comment,
                case_sensitive: caseSensitive,
                use_regex: lore.useRegex ?? false,
            } as LorebookEntry,
            mode: lore.mode ?? "normal",
            folder: lore.folder,
        })
    }
    char.loreExt ??= {}

    char.loreExt.risu_fullWordMatching = char.loreSettings?.fullWordMatching ?? false

    const card:CharacterCardV3 = {
        spec: "chara_card_v3",
        spec_version: "3.0",
        data: {
            name: char.name,
            description: char.desc ?? '',
            personality: char.personality ?? '',
            scenario: char.scenario ?? '',
            first_mes: char.firstMessage ?? '',
            mes_example: char.exampleMessage ?? '',
            creator_notes: char.creatorNotes ?? '',
            system_prompt: char.systemPrompt ?? '',
            post_history_instructions: char.replaceGlobalNote ?? '',
            alternate_greetings: char.alternateGreetings ?? [],
            character_book: {
                scan_depth: char.loreSettings?.scanDepth,
                token_budget: char.loreSettings?.tokenBudget,
                recursive_scanning: char.loreSettings?.recursiveScanning,
                extensions: char.loreExt ?? {},
                entries: charBook
            },
            tags: char.tags ?? [],
            creator: char.additionalData?.creator ?? '',
            character_version: `${char.additionalData?.character_version}` || '',
            extensions: {
                risuai: {
                    bias: char.bias,
                    viewScreen: char.viewScreen,
                    customScripts: char.customscript,
                    utilityBot: char.utilityBot,
                    sdData: char.sdData,
                    backgroundHTML: char.backgroundHTML,
                    license: char.license,
                    triggerscript: char.triggerscript,
                    additionalText: char.additionalText,
                    virtualscript: '', //removed dude to security issue
                    largePortrait: char.largePortrait,
                    lorePlus: char.lorePlus,
                    inlayViewScreen: char.inlayViewScreen,
                    newGenData: char.newGenData,
                    vits: {},
                    lowLevelAccess: char.lowLevelAccess ?? false,
                    defaultVariables: char.defaultVariables ?? '',
                    prebuiltAssetCommand: char.prebuiltAssetCommand ?? '',
                    prebuiltAssetExclude: char.prebuiltAssetExclude ?? [],
                    prebuiltAssetStyle: char.prebuiltAssetStyle ?? '',
                    toggles: char.customModuleToggle ?? '',
                    moduleNamespace: char.moduleNamespace,
                    hideChatIcon: char.hideChatIcon ?? false
                },
                depth_prompt: char.depth_prompt
            },
            group_only_greetings: char.group_only_greetings ?? [],
            nickname: char.nickname ?? '',
            source: char.source ?? [],
            creation_date: char.creation_date ?? 0,
            modification_date: Math.floor(Date.now() / 1000),
            assets: assets
        }
    }

    if(char.extentions){
        for(const key in char.extentions){
            if(key === 'risuai' || key === 'depth_prompt'){
                continue
            }
            card.data.extensions[key] = char.extentions[key]
        }
    }
    return card
}


export async function shareRisuHub2(char:character, arg:{
    nsfw: boolean,
    tag:string
    license: string
    anon: boolean,
    update: boolean
}) {
    try {
        char = safeStructuredClone(char)
        char.license = arg.license
        let tagList = arg.tag.split(',')
        
        if(arg.nsfw){
            tagList.push("nsfw")
        }
    
        alertWait("Uploading...")
        
    
        let tags = tagList.filter((v, i) => {
            return (!!v) && (tagList.indexOf(v) === i)
        })
        char.tags = tags
    
    
        const writer = new VirtualWriter()
        await exportCharacterCard(char, 'png', {writer: writer})
        const dat = Buffer.from(writer.buf.buffer).toString('base64') + '&' + 'rt.png'

        openURL(`${REALM_SITE_URL}/hub/realm/upload#filedata=${encodeURIComponent(dat)}`)

        let testMode = true
        if(testMode){
            return
        }
    
        const fetchPromise = fetch(realmHubURL + '/hub/realm/upload', {
            method: "POST",
            body: writer.buf.buffer as any,
            headers: {
                "Content-Type": 'image/png',
                "x-risu-api-version": "4",
                "x-risu-token": getDatabase()?.account?.token,
                'x-risu-username': arg.anon ? '' : (getDatabase()?.account?.id),
                'x-risu-debug': 'true',
                'x-risu-update-id': arg.update ? (char.realmId ?? 'null') : 'null'
            }
        })
    
    
        const res = await fetchPromise
    
        if(res.status !== 200){
            alertError(await res.text())
        }
        else{
            const resJSON = await res.json()
            alertMd(resJSON.message)
            const currentChar = getCurrentCharacter()
            if(currentChar.type === 'group'){
                return
            }
            currentChar.realmId = resJSON.id
            setCurrentCharacter(currentChar)
        }   
    } catch (error) {
        alertError(error)
    }

}

export type hubType = {
    name:string
    desc: string
    download: string,
    id: string,
    img: string
    tags: string[],
    viewScreen: "none" | "emotion" | "imggen"
    hasLore:boolean
    hasEmotion:boolean
    hasAsset:boolean
    creator?:string
    creatorName?:string
    hot:number
    license:string
    authorname?:string
    original?:string
    type:string
    hidden?:boolean
}

export let hubAdditionalHTML = ''

export async function getRisuHub(arg:{
    search:string,
    page:number,
    nsfw:boolean
    sort:string
}):Promise<hubType[]> {
    try {
        arg.search += ' __shared'
        const stringArg = `search==${arg.search}&&page==${arg.page}&&nsfw==${arg.nsfw}&&sort==${arg.sort}&&web==${(!isNodeServer && !isTauri) ? 'web' : 'other'}`

        const da = await fetch(realmHubURL + '/realm/' + encodeURIComponent(stringArg) + "?cache=30", {
            headers: {
                "x-risuai-info": appVer + ';' + (isNodeServer ? 'node' : (isTauri ? 'tauri' : 'web'))
            }
        })
        if(da.status !== 200){
            return []
        }
        const jso = await da.json()
        if(Array.isArray(jso)){
            return jso
        }
        hubAdditionalHTML = jso.additionalHTML || hubAdditionalHTML
        return jso.cards
    } catch (error) {
        return[]
    }
}

export async function downloadRisuHub(id:string, arg:{
    forceRedirect?: boolean
} = {}) {
    try {
        if(!arg.forceRedirect){
            if(!(await alertRisuServiceTOS())){
                return
            }
            alertStore.set({
                type: "wait",
                msg: "Downloading..."
            })
        }
        const res = await fetch(`${REALM_SITE_URL}/api/v1/download/dynamic/` + id + '?cors=true', {
            headers: {
                "x-risu-api-version": "4"
            }
        })
        if(res.status !== 200){
            alertError(await res.text())
            return
        }

        if(res.headers.get('content-type') === 'image/png' || res.headers.get('content-type') === 'application/zip' || res.headers.get('content-type') === 'application/charx'){
            let db = getDatabase()
            let importedIndex: number | null
            if(res.headers.get('content-type') === 'application/zip' || res.headers.get('content-type') === 'application/charx'){
                importedIndex = await importCharacterProcess({
                    name: 'realm.charx',
                    data: new Uint8Array(await res.arrayBuffer()),
                    lightningRealmImport: db.lightningRealmImport,
                })
            }
            else{
                importedIndex = await importCharacterProcess({
                    name: 'realm.png',
                    data: res.body,
                    lightningRealmImport: db.lightningRealmImport,
                })
            }
            const characterId = importedIndex === null
                ? undefined
                : getDatabase().characters[importedIndex]?.chaId
            checkCharOrder()
            db = getDatabase()
            if(characterId && (db.goCharacterOnImport || arg.forceRedirect)){
                const index = db.characters.findIndex((character) => character.chaId === characterId)
                await changeChar(index)
            }   
            return
        }
    
        const result = await res.json()
        const data:CharacterCardV3 = result.card
        const img:string = result.img

        data.data.extensions.risuRealmImportId = id
    
        const characterId = await importCharacterCardSpec(data, await getHubResources(img), 'hub')
        checkCharOrder()
        let db = getDatabase()
        if(characterId && (db.goCharacterOnImport || arg.forceRedirect)){
            const index = db.characters.findIndex((character) => character.chaId === characterId)
            await changeChar(index)
            alertStore.set({
                type: 'none',
                msg: ''
            })
        }
    } catch (error) {
        console.error(error)
        console.log(error.stack)
        alertError("Error while importing")
    }
}

export async function getHubResources(id:string) {
    const res = await fetch(`${realmHubURL}/resource/${id}`)
    if(res.status !== 200){
        throw (await res.text())
    }
    return Buffer.from(await (res).arrayBuffer())
}

export function isCharacterHasAssets(char:character|groupChat){
    if(char.type === 'group'){
        return false
    }

    if(char.additionalAssets && char.additionalAssets.length > 0){
        return true
    }

    if(char.emotionImages && char.emotionImages.length > 0){
        return true
    }

    if(char.ccAssets && char.ccAssets.length > 0){
        return true
    }

    return false
}


export type CharacterCardV2Risu = {
    spec: 'chara_card_v2'
    spec_version: '2.0' // May 8th addition
    data: {
        name: string
        description: string
        personality: string
        scenario: string
        first_mes: string
        mes_example: string
        creator_notes: string
        system_prompt: string
        post_history_instructions: string
        alternate_greetings: string[]
        character_book?: CharacterBook
        tags: string[]
        creator: string
        character_version: string
        extensions: {
            risuai?:{
                emotions?:[string, string][]
                bias?:[string, number][],
                viewScreen?: any,
                customScripts?:customscript[]
                utilityBot?: boolean,
                sdData?:[string,string][],
                additionalAssets?:[string,string,string][],
                backgroundHTML?:string,
                license?:string,
                triggerscript?:triggerscript[]
                private?:boolean
                additionalText?:string
                virtualscript?:string
                largePortrait?:boolean
                lorePlus?:boolean
                inlayViewScreen?:boolean
                newGenData?: {
                    prompt: string,
                    negative: string,
                    instructions: string,
                    emotionInstructions: string,
                },
                vits?: {[key:string]:string}
            }
            depth_prompt?: { depth: number, prompt: string }
        }
    }
}  


interface OldTavernChar{
    avatar: "none"
    chat: string
    create_date: string
    description: string
    first_mes: string
    mes_example: string
    name: string
    personality: string
    scenario: string
    talkativeness: "0.5"
    spec_version?: '1.0'
}
type CharacterBook = {
    name?: string
    description?: string
    scan_depth?: number // agnai: "Memory: Chat History Depth"
    token_budget?: number // agnai: "Memory: Context Limit"
    recursive_scanning?: boolean // no agnai equivalent. whether entry content can trigger other entries
    extensions: Record<string, any>
    entries: Array<charBookEntry>
  }

interface charBookEntry{
    keys: Array<string>
    content: string
    extensions: Record<string, any>
    enabled: boolean
    insertion_order: number // if two entries inserted, lower "insertion order" = inserted higher

    // FIELDS WITH NO CURRENT EQUIVALENT IN SILLY
    name?: string // not used in prompt engineering
    priority?: number // if token budget reached, lower priority value = discarded first

    // FIELDS WITH NO CURRENT EQUIVALENT IN AGNAI
    id?: number // not used in prompt engineering
    comment?: string // not used in prompt engineering
    selective?: boolean // if `true`, require a key from both `keys` and `secondary_keys` to trigger the entry
    secondary_keys?: Array<string> // see field `selective`. ignored if selective == false
    constant?: boolean // if true, always inserted in the prompt (within budget limit)
    position?: 'before_char' | 'after_char' // whether the entry is placed before or after the character defs
    case_sensitive?:boolean
    use_regex?:boolean
    mode?: string // Risuai mode field
    folder?: string // Risuai folder field
}

interface RccCardMetaData{
    usePassword?: boolean
}
