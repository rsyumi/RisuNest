import { tick } from 'svelte'
import { get } from 'svelte/store'
import { alertError } from '../alert'
import { alertStore } from '../alertQueue'
import { isAppLockInputBlocked, waitForAppLockInputUnblocked } from '../appLock/input'
import { isTauri } from '../platform'
import { DBState, loadedStore, selectedCharID, settingsOpen, SettingsMenuIndex } from '../stores.svelte'
import { MAX_CONTENT_METADATA_BYTES } from '../storage/contentImportLimits'
import { presentFileOperationError } from '../storage/fileOperationErrorPresentation'
import { copyFileToContentInbox, openContentInbox } from '../storage/nativeContentFile'
import {
    nativeFileOperation,
    nativeFileOperationOutcome,
    NativeFileOperationBusyError,
    runSharedNativeFileOperation,
    waitForNativeFileOperationIdle,
    type SharedNativeFileOperationContext,
} from '../storage/nativeFileJobManager'
import { NativeFileJobError } from '../storage/nativeFileJobs'
import { describeIntakeItem, importIntakeItem, importIntakeRows, type ImportIntakeDecision, type ImportIntakeRow } from './batch'
import { classifyNativeImportSources, type NativeImportIntakeClassification, type NativeImportIntakeSource } from './classify'
import { installImportDropListeners } from './drop'
import { inspectPluginFile, requestPluginInstall } from './pluginInstall'
import { importIntakeRefusalText } from './refusalText'
import { openImportSheet } from './sheetStore'
import {
    DROP_COPIED_EXTENSIONS,
    DROP_SCRIPT_EXTENSIONS,
    ImportSourceLedger,
    importSourceName,
    lowercaseExtension,
    nativeImportSource,
} from './sources'
import type {
    ImportIntakeDelivery,
    ImportIntakeItem,
    ImportIntakeRefusal,
    ImportIntakeSource,
    ImportSheetRequest,
} from './types'

const MODULE_SETTINGS_MENU_INDEX = 14

type DeliveryPlan =
    | { type: 'import'; item: ImportIntakeItem; decision: ImportIntakeDecision }
    | { type: 'sheet'; items: ImportIntakeItem[] }
    | { type: 'backup'; item: ImportIntakeItem }
    | { type: 'plugin'; item: ImportIntakeItem }
    | { type: 'refused'; item: ImportIntakeItem }

interface DeliveryState {
    id: number
    delivery: ImportIntakeDelivery
    /** A drop made while the module settings page was open points CharX and lorebook files at modules. */
    moduleScreen: boolean
    ledger: ImportSourceLedger
    startedAt: number
}

let deliveryCount = 0
let deliveryQueue: Promise<void> = Promise.resolve()
const dropModuleScreens = new WeakMap<ImportIntakeDelivery, boolean>()

/** Starts the intake listeners; returns their disposer. */
export function installImportIntake(): () => void {
    if (!isTauri) return () => {}
    return installImportDropListeners((delivery) => {
        void submitImportDelivery(delivery)
    })
}

/** Queues files that arrived together and handles them once the app is idle. */
export function submitImportDelivery(delivery: ImportIntakeDelivery): Promise<void> {
    if (delivery.origin === 'drop') {
        dropModuleScreens.set(delivery, get(settingsOpen) && get(SettingsMenuIndex) === MODULE_SETTINGS_MENU_INDEX)
    }
    const run = deliveryQueue.then(() => handleDelivery(delivery, ++deliveryCount))
    deliveryQueue = run.catch(() => {})
    return run
}

/** Resolves once the app has finished loading. */
function waitForLoaded(): Promise<void> {
    return new Promise(resolve => {
        let stop: (() => void) | undefined
        stop = loadedStore.subscribe(loaded => {
            if (!loaded) return
            stop?.()
            resolve()
        })
        if (get(loadedStore)) stop()
    })
}

/**
 * Waits until the app has loaded, the app lock is gone, no alert dialog is open or queued, no file operation runs
 * and no outcome is shown.
 */
async function waitForImportIdle(): Promise<void> {
    while (!get(loadedStore) || isAppLockInputBlocked() || alertStore.hasDialogs() || get(nativeFileOperation) || get(nativeFileOperationOutcome)) {
        // The app lock comes up once loading ends, so a file opened at startup waits for both.
        await waitForLoaded()
        await tick()
        await waitForAppLockInputUnblocked()
        await alertStore.idle()
        await waitForNativeFileOperationIdle()
    }
}

/** Runs a shared import operation once the app is idle, waiting again when another one started first. */
async function runIntakeOperation<T>(
    key: string,
    operation: (context: SharedNativeFileOperationContext) => Promise<T>,
): Promise<T> {
    while (true) {
        await waitForImportIdle()
        let started = false
        try {
            return await runSharedNativeFileOperation('import', key, (context) => {
                started = true
                return operation(context)
            }, { presentation: 'dialog', format: 'content' })
        } catch (error) {
            if (started || !(error instanceof NativeFileOperationBusyError)) throw error
        }
    }
}

/** Runs a restore once the app is idle, again when another file operation started first. */
async function restoreWhenIdle(
    restore: () => Promise<unknown>,
    present: (error: unknown, startedAt: number) => void,
): Promise<void> {
    while (true) {
        await waitForImportIdle()
        const startedAt = Date.now()
        try {
            await restore()
            return
        } catch (error) {
            // The restore asks for confirmation inside its operation, so a busy refusal has asked nothing yet.
            if (error instanceof NativeFileOperationBusyError) continue
            present(error, startedAt)
            return
        }
    }
}

function needsCopy(source: ImportIntakeSource): boolean {
    return source.type === 'file' && DROP_COPIED_EXTENSIONS.has(lowercaseExtension(source.file.name))
}

async function handleDelivery(delivery: ImportIntakeDelivery, id: number): Promise<void> {
    if (!delivery.sources.length) return
    const state: DeliveryState = {
        id,
        delivery,
        moduleScreen: delivery.origin === 'drop' && (dropModuleScreens.get(delivery) ?? false),
        ledger: new ImportSourceLedger(),
        startedAt: Date.now(),
    }
    try {
        await waitForImportIdle()
        if (!delivery.sources.some(needsCopy)) {
            await carryOut(planDelivery(await readDelivery(state), state), state)
            return
        }
        const planned: { plan?: DeliveryPlan } = {}
        await runIntakeOperation(`import-intake:${id}:read`, async (context) => {
            const plan = planDelivery(await readDelivery(state, context), state)
            if (plan.type !== 'import') {
                planned.plan = plan
                return null
            }
            // A direct import continues in the dialog that showed the copy.
            context.setSource(describeIntakeItem(plan.item))
            return importIntakeItem(plan.item, plan.decision, context)
        })
        if (planned.plan) await carryOut(planned.plan, state)
    } catch (error) {
        presentFileOperationError('import', error, state.startedAt)
    } finally {
        await state.ledger.release()
    }
}

/**
 * Classifies every file of the delivery, in delivery order. Dropped files are first copied into the
 * inbox inside the caller's operation, whose cancel stops before the next file; a file that cannot
 * be copied or read is refused.
 */
async function readDelivery(state: DeliveryState, context?: SharedNativeFileOperationContext): Promise<ImportIntakeItem[]> {
    const items: ImportIntakeItem[] = []
    const inbox: { path?: string } = {}
    for (const [index, source] of state.delivery.sources.entries()) {
        context?.signal.throwIfAborted()
        items.push(await readSource(state, `${state.id}:${index}`, source, inbox, context))
    }
    context?.signal.throwIfAborted()
    if (items.length > 1) {
        // A backup replaces the whole library, so it is never imported together with other files.
        for (const [index, item] of items.entries()) {
            if (item.kind !== 'backup' || item.refusal) continue
            items[index] = { ...item, refusal: { code: 'backup-with-other-files' } }
            state.ledger.markRefusedBackup(item.source)
        }
    }
    return items
}

async function readSource(
    state: DeliveryState,
    id: string,
    source: ImportIntakeSource,
    inbox: { path?: string },
    context?: SharedNativeFileOperationContext,
): Promise<ImportIntakeItem> {
    const name = importSourceName(source)
    if (source.type === 'failed') {
        const error = new NativeFileJobError(source.code, `${name}: ${source.code}`)
        return { id, name, source, kind: 'unsupported', refusal: { code: 'read-failed', error } }
    }
    if (source.type !== 'file') {
        if (source.type === 'androidSpool') state.ledger.addSpool(source.spool)
        const item = { id, name, source, ...(source.type === 'androidSpool' ? { bytes: source.spool.bytes } : {}) }
        return classify(item, { source: nativeImportSource(source)!, displayName: name }, state.moduleScreen)
    }
    const item = { id, name, bytes: source.file.size, source }
    if (DROP_SCRIPT_EXTENSIONS.has(lowercaseExtension(name))) return readPluginCandidate(item, source.file)
    if (!needsCopy(source)) return { ...item, kind: 'unsupported', refusal: { code: 'unsupported-format' } }
    if (!context) throw new Error('A dropped file is copied inside an operation')
    // The JSON importers stop at the import limit, so such a file is refused before it is copied.
    if (source.file.size > MAX_CONTENT_METADATA_BYTES && await opensJsonObject(source.file)) {
        return { ...item, kind: 'unsupported', refusal: tooLargeRefusal() }
    }
    context.setSource({ name, bytes: source.file.size })
    let inboxPath: string
    try {
        inbox.path ??= await openContentInbox()
        inboxPath = await copyFileToContentInbox(inbox.path, source.file, { signal: context.signal, onStatus: context.onStatus })
    } catch (error) {
        if (context.signal.aborted) throw error
        return { ...item, kind: 'unsupported', refusal: { code: 'read-failed', error } }
    }
    state.ledger.addCopy(inboxPath)
    const copied = { ...item, source: { ...source, inboxPath } }
    return classify(copied, { source: { type: 'desktopPath', path: inboxPath }, displayName: name }, state.moduleScreen)
}

/** Whether the first byte after leading whitespace opens a JSON object, which is what makes the native classifier read JSON. */
async function opensJsonObject(file: File): Promise<boolean> {
    try {
        const prefix = new Uint8Array(await file.slice(0, 64 * 1024).arrayBuffer())
        return prefix.find(byte => ![0x20, 0x09, 0x0a, 0x0d].includes(byte)) === 0x7b
    } catch {
        // The copy that follows reports a file that cannot be read.
        return false
    }
}

async function classify(
    item: Omit<ImportIntakeItem, 'kind'>,
    request: NativeImportIntakeSource,
    moduleScreen: boolean,
): Promise<ImportIntakeItem> {
    let result: NativeImportIntakeClassification | undefined
    try {
        [result] = await classifyNativeImportSources([request])
    } catch (error) {
        result = { kind: 'unsupported', refusal: 'read-failed', message: String(error) }
    }
    return classifiedItem(item, result, moduleScreen)
}

function classifierRefusal(result: NativeImportIntakeClassification | undefined): ImportIntakeRefusal {
    if (!result || result.refusal === 'read-failed') {
        return { code: 'read-failed', error: new NativeFileJobError('source-read-failed', result?.message ?? '') }
    }
    if (result.refusal === 'too-large') return tooLargeRefusal()
    return { code: result.kind === 'unsupported' && result.refusal ? result.refusal : 'unsupported-format' }
}

function classifiedItem(
    item: Omit<ImportIntakeItem, 'kind'>,
    result: NativeImportIntakeClassification | undefined,
    moduleScreen: boolean,
): ImportIntakeItem {
    const bytes = item.bytes ?? result?.bytes
    const known: Omit<ImportIntakeItem, 'kind'> = {
        ...item,
        ...(bytes === undefined ? {} : { bytes }),
        ...(result?.format ? { format: result.format } : {}),
    }
    // A script that arrives by file association or share is never installed.
    if (!result || result.kind === 'unsupported' || result.kind === 'script') {
        return { ...known, kind: 'unsupported', refusal: classifierRefusal(result) }
    }
    if (result.kind === 'character' && result.format === 'charx') {
        return { ...known, kind: 'character', moduleHint: /\.module\.charx$/i.test(item.name) || moduleScreen }
    }
    return { ...known, kind: result.kind }
}

/** A file over the import limit, refused before it is read. */
function tooLargeRefusal(): ImportIntakeRefusal {
    return { code: 'read-failed', error: new NativeFileJobError('payload-too-large', 'File exceeds the import limit', [], 'content') }
}

async function readPluginCandidate(item: Omit<ImportIntakeItem, 'kind'>, file: File): Promise<ImportIntakeItem> {
    const isTypescript = lowercaseExtension(file.name) === 'ts'
    const candidate = { ...item, format: isTypescript ? 'ts' as const : 'js' as const }
    if (file.size > MAX_CONTENT_METADATA_BYTES) return { ...candidate, kind: 'unsupported', refusal: tooLargeRefusal() }
    try {
        const code = await file.text()
        const inspected = inspectPluginFile(file.name, code, isTypescript)
        if ('refusal' in inspected) {
            return {
                ...candidate,
                kind: inspected.refusal.code === 'plugin-api-version' ? 'plugin' : 'unsupported',
                refusal: inspected.refusal,
            }
        }
        return { ...candidate, kind: 'plugin', plugin: { header: inspected.header, code, isTypescript } }
    } catch (error) {
        return { ...candidate, kind: 'unsupported', refusal: { code: 'read-failed', error } }
    }
}

/** The selected character a lorebook can go to; the playground is not one. */
function lorebookCharacter(): { chaId: string; name: string } | undefined {
    const character = DBState.db.characters?.[get(selectedCharID)]
    if (!character || character.chaId === '§playground') return undefined
    return { chaId: character.chaId, name: character.name }
}

function planDelivery(items: ImportIntakeItem[], state: DeliveryState): DeliveryPlan {
    if (items.length !== 1) return { type: 'sheet', items }
    const [item] = items
    if (item.refusal || item.kind === 'unsupported') return { type: 'refused', item }
    switch (item.kind) {
        case 'backup':
            return { type: 'backup', item }
        case 'plugin':
            return state.delivery.origin === 'drop' ? { type: 'plugin', item } : { type: 'refused', item }
        case 'character':
            return item.format === 'charx' && item.moduleHint
                ? { type: 'sheet', items }
                : { type: 'import', item, decision: { charxAs: 'character' } }
        case 'lorebook':
            return lorebookCharacter()
                ? { type: 'sheet', items }
                : { type: 'import', item, decision: { lorebookTarget: { type: 'new-module' } } }
        default:
            return { type: 'import', item, decision: {} }
    }
}

function sheetRequest(items: ImportIntakeItem[], state: DeliveryState): ImportSheetRequest {
    const character = lorebookCharacter()
    const defaults: ImportSheetRequest['defaults'] = {}
    for (const item of items) {
        if (item.refusal) continue
        if (item.kind === 'character' && item.format === 'charx') {
            defaults[item.id] = { charxAs: item.moduleHint ? 'module' : 'character' }
        } else if (item.kind === 'lorebook') {
            defaults[item.id] = {
                lorebookTarget: character && !state.moduleScreen ? { type: 'character', ...character } : { type: 'new-module' },
            }
        }
    }
    return {
        items,
        ...(character ? { lorebookCharacter: character } : {}),
        defaults,
        installPlugin: async (item) => await requestPluginInstall(item) === 'installed',
    }
}

async function carryOut(plan: DeliveryPlan, state: DeliveryState): Promise<void> {
    switch (plan.type) {
        case 'refused': {
            const reason = importIntakeRefusalText(plan.item.refusal ?? { code: 'unsupported-format' })
            alertError(`${plan.item.name}\n${reason}`)
            return
        }
        case 'import':
            await runIntakeOperation(`import-intake:${state.id}:import`, (context) => {
                context.setSource(describeIntakeItem(plan.item))
                return importIntakeItem(plan.item, plan.decision, context)
            })
            return
        case 'sheet': {
            await waitForImportIdle()
            const decision = await openImportSheet(sheetRequest(plan.items, state))
            const rows: ImportIntakeRow[] = (decision ?? []).flatMap(({ id, ...choice }) => {
                const item = plan.items.find(candidate => candidate.id === id)
                return item && !item.refusal && ['character', 'module', 'lorebook', 'preset'].includes(item.kind)
                    ? [{ item, decision: choice }]
                    : []
            })
            if (!rows.length) return
            await runIntakeOperation(`import-intake:${state.id}:batch`, context => importIntakeRows(context, rows))
            return
        }
        case 'backup':
            await restoreBackup(plan.item)
            return
        case 'plugin':
            await waitForImportIdle()
            await requestPluginInstall(plan.item)
            return
    }
}

/** The delivery keeps the source, so a restore refused before it took the source still releases it. */
async function restoreBackup(item: ImportIntakeItem): Promise<void> {
    const source = item.source
    if (source.type === 'androidSpool') {
        const { restoreAndroidOpenedBackupSource, showAndroidFileError } = await import('../storage/androidRisuSaveRouteProduction.svelte')
        await restoreWhenIdle(() => restoreAndroidOpenedBackupSource({
            source: { type: 'androidSpool', token: source.spool.token },
            displayName: source.spool.displayName,
            bytes: source.spool.bytes,
        }), showAndroidFileError)
    } else {
        const native = nativeImportSource(source)
        if (native?.type !== 'desktopPath') return
        const { restoreBackupFromNativeSource } = await import('../storage/portableBackupFileRouteProduction.svelte')
        // The item name, not the path's, since an inbox copy or an Android copy prefixes the file it holds.
        const sourceInfo = { name: item.name, ...(item.bytes === undefined ? {} : { bytes: item.bytes }) }
        await restoreWhenIdle(() => restoreBackupFromNativeSource(native, { sourceInfo }),
            (error, startedAt) => presentFileOperationError('import', error, startedAt))
    }
    // A portable restore can still read its source while its outcome offers to retry the missing
    // bodies, so the source stays until that outcome and any retry from it are gone.
    await waitForNativeFileOperationIdle()
}
