import {
    writeFile,
    BaseDirectory,
    readFile,
    exists,
    mkdir,
    readDir,
    remove
} from "@tauri-apps/plugin-fs"
import { changeFullscreen, sleep } from "./util"
import { get } from "svelte/store";
import { setDatabase, getDatabase, type Database } from "./storage/database.svelte";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { checkRisuUpdate } from "./update";
import { MobileGUI, botMakerMode, selectedCharID, loadedStore, DBState, LoadingStatusState } from "./stores.svelte";
import { loadPlugins } from "./plugins/plugins.svelte";
import { alertError, alertInput, alertMd, alertSelect, alertTOS, waitAlert } from "./alert";
import { checkDriverInit } from "./drive/drive";
import { characterURLImport } from "./characterCards";
import { loadRisuAccountData } from "./drive/accounter";
import { decodeRisuSave } from "./storage/risuSave";
import { updateAnimationSpeed } from "./gui/animation";
import { updateColorScheme, updateTextThemeAndCSS } from "./gui/colorscheme";
import { language } from "src/lang";
import { startObserveDom } from "./observer.svelte";
import { updateGuisize } from "./gui/guisize";
import { initMobileGesture } from "./hotkey";
import { moduleUpdate } from "./process/modules";
import { AccountStorage } from "./storage/accountStorage";
import {
    getAccountColdStorageItem,
    getColdStorageItem,
    makeColdData,
    setAccountColdStorageItem,
} from "./process/coldstorage.svelte";
import { getRemoteSaveCleanupAction, getRemoteSavePayloadName } from "./storage/remoteSaveCleanup";
import {
    forageStorage,
    saveDb,
    getUncleanables,
    getBasename,
    setUsingSw
} from "./globalApi.svelte";
import { isTauri, isTauriDesktop } from "./platform";
import { registerModelDynamic } from "./model/modellist";
import { convertFileSrc } from "@tauri-apps/api/core";
import { appDataDir, join } from "@tauri-apps/api/path";
import {
    checkNewFormat as migrateDatabaseFormat,
    prepareDatabaseForPersistence,
} from "./storage/databasePreparation";
import { bootstrapPersistentDatabase } from "./storage/persistentBootstrap";
import {
    getPersistentDataRuntime,
    initializeActiveWorkingSet,
    configurePersistentDataRuntime,
} from "./storage/persistentDataRuntime.svelte";
import { registerLifecycleCommitListeners } from "./storage/lifecycleCommit";
import { resolveBlobStore } from "./storage/platformBlobStore";
import {
    OfficialAccountSnapshotAdapter,
    createOfficialAssociationMarkers,
} from "./storage/sync/officialAccountSnapshot";
import { initializePersistentStorage } from "./storage/persistentStorageRuntime";
import {
    initializeOfficialAccountBootstrap,
    publishOfficialRevisionIfChanged,
} from "./storage/sync/officialAccountBootstrap";
import { createAccountScopedOfficialAssetLedger } from "./storage/sync/officialAssetLedger";
import {
    configureOfficialAccountAssetReader,
    createStructuredAccountAssetReader,
} from "./storage/accountAssetAccess";
export { assignIds } from "./storage/databasePreparation";

const appWindow = isTauri ? getCurrentWebviewWindow() : null
let disposeLifecycleCommitListeners: (() => void) | undefined

/**
 * Loads the application data.
 */
export async function loadData() {
    if (get(loadedStore)) return
    try {
        if (isTauri) {
            LoadingStatusState.text = 'Checking Files...'
            if (isTauriDesktop) appWindow.maximize()
            if (!await exists('', { baseDir: BaseDirectory.AppData })) {
                await mkdir('', { baseDir: BaseDirectory.AppData })
            }
            if (!await exists('assets', { baseDir: BaseDirectory.AppData })) {
                await mkdir('assets', { baseDir: BaseDirectory.AppData })
            }
        } else {
            await forageStorage.Init()
        }

        await initializePersistentStorage()
        const runtime = getPersistentDataRuntime()
        const local = await bootstrapPersistentDatabase({
            store: runtime.store,
            prepareDatabase: prepareDatabaseForPersistence,
        })
        setDatabase(local.database)
        const accountStorage = new AccountStorage()
        const officialAdapter = new OfficialAccountSnapshotAdapter({
            store: runtime.store,
            resolveBlobs: resolveBlobStore,
            account: accountStorage,
            cold: {
                readRemote: getAccountColdStorageItem,
                async writeRemote(key, value, signal) {
                    if (!await setAccountColdStorageItem(key, value, signal)) {
                        throw new Error(`Failed to write official cold payload: ${key}`)
                    }
                },
                readLocal: (key) => getColdStorageItem(key, { accountFallback: true }),
            },
            prepareCandidate: prepareDatabaseForPersistence,
            markPublished: () => undefined,
            ledger: createAccountScopedOfficialAssetLedger(
                localStorage,
                () => getDatabase().account?.id,
            ),
            association: createOfficialAssociationMarkers(localStorage),
        })
        const accountBootstrap = await initializeOfficialAccountBootstrap({
            local,
            store: runtime.store,
            adapter: officialAdapter,
            readRemoteDatabase: () => accountStorage.readItem('database/database.bin', {
                progress: (value) => {
                    LoadingStatusState.text =
                        `Loading Remote Save File ${(value * 100).toFixed(2)}%`
                },
            }),
            markers: localStorage,
            accountMode: {
                get isAccount() {
                    return forageStorage.isAccount
                },
                set isAccount(enabled) {
                    forageStorage.setAccountModeForSession(enabled)
                },
            },
            configurePublisher: (officialPublisher) => {
                configurePersistentDataRuntime({ officialPublisher })
            },
            assetReader: createStructuredAccountAssetReader(accountStorage),
            configureAssetReader: configureOfficialAccountAssetReader,
            chooseExistingRemote: async () => await alertSelect([
                language.loadDataFromAccount,
                language.saveCurrentDataToAccount,
            ]) === '0' ? 'pull' : 'push',
            confirmInitialPush: async () =>
                await alertInput('to overwrite your data, type "RISUAI"') === 'RISUAI',
            installDatabase: setDatabase,
            initializeWorkingSet: (database) => initializeActiveWorkingSet(database),
            onRemoteError: (error) => {
                console.error(error)
                alertError(error instanceof Error ? error : String(error))
            },
            onPullSkipped: ({ conflict }) => {
                console.warn(conflict
                    ? 'Official account pull skipped: local revisions were never published and the remote save also changed. Keeping local data; the next publish overwrites the remote save.'
                    : 'Official account pull skipped: local revisions were never published. Keeping local data until the next publish.')
            },
        })
        disposeLifecycleCommitListeners ??= registerLifecycleCommitListeners()

        if (isTauriDesktop) {
            LoadingStatusState.text = 'Checking Update...'
            await checkRisuUpdate()
            await changeFullscreen()
        }

        if (!isTauri) {
            LoadingStatusState.text = 'Checking Drive Sync...'
            if (await checkDriverInit()) return
            LoadingStatusState.text = 'Checking Service Worker...'
            if (navigator.serviceWorker) {
                setUsingSw(true)
                await registerSw()
            } else {
                setUsingSw(false)
            }
            if (getDatabase().didFirstSetup) characterURLImport()
        }

        LoadingStatusState.text = 'Checking For Format Update...'
        const coldStorageChanged = await makeColdData()
        await publishOfficialRevisionIfChanged(
            coldStorageChanged && accountBootstrap.officialEnabled,
            officialAdapter,
            runtime.revision,
        )

        LoadingStatusState.text = 'Loading Plugins...'
        try {
            await loadPlugins()
        } catch (error) {
            console.error(error)
        }
        if (getDatabase().account) {
            LoadingStatusState.text = 'Checking Account Data...'
            try {
                await loadRisuAccountData()
            } catch (error) {
                console.error(error)
            }
        }
        try {
            const isInStandaloneMode = window.matchMedia('(display-mode: standalone)').matches ||
                (window.navigator as Navigator & { standalone?: boolean }).standalone ||
                document.referrer.includes('android-app://')
            if (isInStandaloneMode) await navigator.storage.persist()
        } catch {}

        const database = getDatabase()
        LoadingStatusState.text = 'Updating States...'
        updateColorScheme()
        updateTextThemeAndCSS()
        updateAnimationSpeed()
        updateHeightMode()
        updateErrorHandling()
        updateGuisize()
        if (!localStorage.getItem('nightlyWarned') && window.location.hostname === 'nightly.risuai.xyz') {
            alertMd(language.nightlyWarning)
            await waitAlert()
            localStorage.setItem('nightlyWarned', '')
        }
        if (database.botSettingAtStart) botMakerMode.set(true)
        if (
            (database.betaMobileGUI && window.innerWidth <= 800) ||
            import.meta.env.VITE_RISU_LITE === 'TRUE'
        ) {
            initMobileGesture()
            MobileGUI.set(true)
        }
        loadedStore.set(true)
        selectedCharID.set(-1)
        startObserveDom()
        registerModelDynamic()
        await saveDb()
        moduleUpdate()
        cleanChunks()
        void alertTOS().then((accepted) => {
            if (accepted === false) location.reload()
        })
    } catch (error) {
        alertError(error)
    }
}


/**
 * Registers the service worker and initializes it.
 */
async function registerSw() {
    await navigator.serviceWorker.register("/sw.js", {
        scope: "/"
    });
    await sleep(100);
    const da = await fetch('/sw/init');
    if (!(da.status >= 200 && da.status < 300)) {
        location.reload();
    }
}

/**
 * Updates the error handling by adding custom handlers for errors and unhandled promise rejections.
 */
function updateErrorHandling() {
    const errorHandler = (event: ErrorEvent) => {
        console.error(event.error);
        if(!(event.error.target instanceof Worker)){
            alertError(event.error);            
        }
    };
    const rejectHandler = (event: PromiseRejectionEvent) => {
        console.error(event.reason);
        alertError(event.reason);
    };
    window.addEventListener('error', errorHandler);
    window.addEventListener('unhandledrejection', rejectHandler);
}

/**
 * Updates the height mode of the document based on the value stored in the database.
 */
function updateHeightMode() {
    const db = getDatabase()
    const root = document.querySelector(':root') as HTMLElement;
    switch (db.heightMode) {
        case 'auto':
            root.style.setProperty('--risu-height-size', '100%');
            break
        case 'vh':
            root.style.setProperty('--risu-height-size', '100vh');
            break
        case 'dvh':
            root.style.setProperty('--risu-height-size', '100dvh');
            break
        case 'lvh':
            root.style.setProperty('--risu-height-size', '100lvh');
            break
        case 'svh':
            root.style.setProperty('--risu-height-size', '100svh');
            break
        case 'percent':
            root.style.setProperty('--risu-height-size', '100%');
            break
    }
}

/**
 * Checks and updates the database format to the latest version.
 */
export async function checkNewFormat(
    db: Database,
    options: { now?: number } = {},
): Promise<Database> {
    return migrateDatabaseFormat(db, options)
}

/**
 * Purges chunks of data that are not needed.
 */
async function cleanChunks(options:{
    cleanColdStorage?: boolean
} = {}) {
    const cleanColdStorage = options.cleanColdStorage ?? false
    const db = getDatabase()
    if (db.account?.useSync) {
        return
    }
    if(db.coldstorage && !cleanColdStorage){
        return
    }

    const uncleanable = new Set(await getUncleanables(db))
    const blobStore = await resolveBlobStore()
    if (isTauri) {
        const assets = await readDir('assets', { baseDir: BaseDirectory.AppData })
        console.log(assets)
        for (const asset of assets) {
            try {
                const n = getBasename(asset.name)
                if (!uncleanable.has(n)) {
                    await blobStore.remove('assets/' + asset.name)
                }
            } catch (error) {
                console.log('error', asset.name)
            }
        }

        
        if(!await exists('remotes', { baseDir: BaseDirectory.AppData })) {
            await mkdir('remotes', { baseDir: BaseDirectory.AppData })
        }

        const remotes = await readDir('remotes', { baseDir: BaseDirectory.AppData })

        const remoteUncleanables = new Set<string>(
            db.characters.map((v) => v.chaId)
        )
        for (const remote of remotes) {
            try {
                const remoteFileName = getBasename(remote.name)
                const remotePayloadName = getRemoteSavePayloadName(remoteFileName)
                if(!remotePayloadName){
                    continue
                }
                const fexists = remoteUncleanables.has(remotePayloadName)
                if(!fexists){

                    const metaPath = 'remotes/' + remote.name + '.meta'
                    let metaExists = false
                    let metaLastUsed:unknown
                    try {
                        metaExists = await exists(metaPath, { baseDir: BaseDirectory.AppData })
                        if (metaExists) {
                            const meta = await readFile(metaPath, { baseDir: BaseDirectory.AppData })
                            const metaJson = JSON.parse(new TextDecoder().decode(meta))
                            metaLastUsed = metaJson.lastUsed
                        }
                    } catch (error) {}

                    const cleanupAction = getRemoteSaveCleanupAction({
                        fileName: remoteFileName,
                        activeCharacterIds: remoteUncleanables,
                        hasMeta: metaExists,
                        metaLastUsed
                    })
                    if(cleanupAction === 'create-meta'){
                        const metaJson = {
                            lastUsed: Date.now()
                        }
                        await writeFile(metaPath, new TextEncoder().encode(JSON.stringify(metaJson)), { baseDir: BaseDirectory.AppData })
                    }
                    else if(cleanupAction === 'delete'){
                        await remove('remotes/' + remote.name, { baseDir: BaseDirectory.AppData })
                        await remove(metaPath, { baseDir: BaseDirectory.AppData })
                    }
                }
            } catch (error) {
                console.log('error', remote.name)
            }
        }
    }
    else {
        const indexes = await forageStorage.keys()
        const characterIds = new Set<string>(
            db.characters.map((v) => v.chaId)
        )
        for (const asset of indexes) {
            if (asset.startsWith('assets/')) {
                const n = getBasename(asset)
                if(!uncleanable.has(n)) {
                    await blobStore.remove(asset)
                }
            }
            else if (asset.endsWith('.meta')){
                continue
            }
            else if (asset.startsWith('remotes/')) {
                const name = getBasename(asset).slice(0, -10) //remove .local.bin
                const exists = characterIds.has(name)
                if(!exists){
                    let okayToDelete = false
                    try {
                        const metaPath = asset + '.meta'
                        const metaExists = (await forageStorage.keys()).includes(metaPath)
                        if (metaExists) {
                            const metaData: Uint8Array = await forageStorage.getItem(metaPath) as unknown as Uint8Array
                            const metaJson = JSON.parse(new TextDecoder().decode(metaData))
                            const lastUsed = metaJson.lastUsed as number
                            if(Date.now() - lastUsed > 1000 * 60 * 60 * 24 * 7) { //not used for 7 days
                                okayToDelete = true
                            }
                        }
                        else{
                            //write meta for next time
                            const metaJson = {
                                lastUsed: Date.now()
                            }
                            await forageStorage.setItem(metaPath, new TextEncoder().encode(JSON.stringify(metaJson)))
                        }
                    } catch (error) {}
                    if (okayToDelete) {
                        await forageStorage.removeItem(asset)
                    }
                }
            }
        }
    }
}


/**
 * Assigns unique IDs to characters and chats.
 */
