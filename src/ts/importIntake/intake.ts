import { get } from 'svelte/store'
import { alertError } from '../alert'
import { alertStore } from '../alertQueue'
import { isAppLockInputBlocked, waitForAppLockInputUnblocked } from '../appLock/input'
import { isTauri } from '../platform'
import { DBState, selectedCharID, settingsOpen, SettingsMenuIndex } from '../stores.svelte'
import { AndroidSafSourceError } from '../storage/androidSafBridge'
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

/** Waits until the app lock is gone, no alert dialog is open or queued, no file operation runs and no outcome is shown. */
async function waitForImportIdle(): Promise<void> {
    while (isAppLockInputBlocked() || alertStore.hasDialogs() || get(nativeFileOperation) || get(nativeFileOperationOutcome)) {
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
            return importIntakeItem(plan.item, plan.decision, context, state.ledger)
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
 * inbox inside the caller's operation; a file that cannot be copied or read is refused.
 */
async function readDelivery(state: DeliveryState, context?: SharedNativeFileOperationContext): Promise<ImportIntakeItem[]> {
    const { delivery, ledger } = state
    const items: ImportIntakeItem[] = []
    const pending: { index: number; item: Omit<ImportIntakeItem, 'kind'>; request: NativeImportIntakeSource }[] = []
    let inbox: string | undefined
    for (const [index, source] of delivery.sources.entries()) {
        const name = importSourceName(source)
        const id = `${state.id}:${index}`
        if (source.type === 'file') {
            const item = { id, name, bytes: source.file.size, source }
            if (needsCopy(source)) {
                if (!context) throw new Error('A dropped file is copied inside an operation')
                context.setSource({ name, bytes: source.file.size })
                try {
                    inbox ??= await openContentInbox()
                    const inboxPath = await copyFileToContentInbox(inbox, source.file, { signal: context.signal, onStatus: context.onStatus })
                    ledger.addCopy(inboxPath)
                    pending.push({
                        index,
                        item: { ...item, source: { ...source, inboxPath } },
                        request: { source: { type: 'desktopPath', path: inboxPath }, displayName: name },
                    })
                } catch (error) {
                    if (context.signal.aborted) throw error
                    items[index] = { ...item, kind: 'unsupported', refusal: { code: 'read-failed', error } }
                }
            } else if (DROP_SCRIPT_EXTENSIONS.has(lowercaseExtension(name))) {
                items[index] = await readPluginCandidate(item, source.file)
            } else {
                items[index] = { ...item, kind: 'unsupported', refusal: { code: 'unsupported-format' } }
            }
            continue
        }
        if (source.type === 'androidSpoolFailure') {
            const error = new AndroidSafSourceError(source.failure.code, `${name}: ${source.failure.code}`)
            items[index] = { id, name, source, kind: 'unsupported', refusal: { code: 'read-failed', error } }
            continue
        }
        if (source.type === 'androidSpool') ledger.addSpool(source.spool)
        pending.push({
            index,
            item: { id, name, source, ...(source.type === 'androidSpool' ? { bytes: source.spool.bytes } : {}) },
            request: { source: nativeImportSource(source)!, displayName: name },
        })
    }
    if (pending.length) {
        let results: NativeImportIntakeClassification[]
        try {
            results = await classifyNativeImportSources(pending.map(entry => entry.request))
        } catch (error) {
            results = pending.map(() => ({ kind: 'unsupported', refusal: 'read-failed', message: String(error) }))
        }
        pending.forEach((entry, position) => {
            items[entry.index] = classifiedItem(entry.item, results[position], state.moduleScreen)
        })
    }
    context?.signal.throwIfAborted()
    if (items.length > 1) {
        // A backup replaces the whole library, so it is never imported together with other files.
        for (const [index, item] of items.entries()) {
            if (item.kind !== 'backup' || item.refusal) continue
            items[index] = { ...item, refusal: { code: 'backup-with-other-files' } }
            ledger.markRefusedBackup(item.source)
        }
    }
    return items
}

function classifierRefusal(result: NativeImportIntakeClassification | undefined): ImportIntakeRefusal {
    if (!result || result.refusal === 'read-failed') {
        return { code: 'read-failed', error: new NativeFileJobError('source-read-failed', result?.message ?? '') }
    }
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

async function readPluginCandidate(item: Omit<ImportIntakeItem, 'kind'>, file: File): Promise<ImportIntakeItem> {
    const isTypescript = lowercaseExtension(file.name) === 'ts'
    const candidate = { ...item, format: isTypescript ? 'ts' as const : 'js' as const }
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

/** The selected character a lorebook can go to. */
function lorebookCharacter(): { chaId: string; name: string } | undefined {
    const character = DBState.db.characters?.[get(selectedCharID)]
    return character ? { chaId: character.chaId, name: character.name } : undefined
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
                return importIntakeItem(plan.item, plan.decision, context, state.ledger)
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
            await runIntakeOperation(`import-intake:${state.id}:batch`, context => importIntakeRows(context, rows, state.ledger))
            return
        }
        case 'backup':
            await restoreBackup(plan.item, state)
            return
        case 'plugin':
            await waitForImportIdle()
            await requestPluginInstall(plan.item)
            return
    }
}

async function restoreBackup(item: ImportIntakeItem, state: DeliveryState): Promise<void> {
    await waitForImportIdle()
    const startedAt = Date.now()
    const source = item.source
    if (source.type === 'androidSpool') {
        state.ledger.claim(source)
        const { restoreAndroidOpenedBackupSource, showAndroidFileError } = await import('../storage/androidRisuSaveRouteProduction.svelte')
        try {
            await restoreAndroidOpenedBackupSource({
                source: { type: 'androidSpool', token: source.spool.token },
                displayName: source.spool.displayName,
            })
        } catch (error) {
            showAndroidFileError(error, startedAt)
        }
        return
    }
    const native = nativeImportSource(source)
    if (native?.type !== 'desktopPath') return
    const { restoreBackupFromNativeSource } = await import('../storage/portableBackupFileRouteProduction.svelte')
    try {
        await restoreBackupFromNativeSource(native, source.type === 'file'
            ? { sourceInfo: { name: item.name, ...(item.bytes === undefined ? {} : { bytes: item.bytes }) } }
            : {})
    } catch (error) {
        presentFileOperationError('import', error, startedAt)
    }
    // A portable restore can still read its source while its outcome offers to retry the missing
    // bodies, so the source stays until that outcome and any retry from it are gone.
    await waitForNativeFileOperationIdle()
}
