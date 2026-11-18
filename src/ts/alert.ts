import { get, writable } from "svelte/store"
import { language } from "../lang"
import { isTauri } from "src/ts/platform"
import { getDatabase, type Message, type MessageGenerationInfo } from "./storage/database.svelte"
import { alertStore as alertStoreImported } from "./stores.svelte"
import { getDeviceMarkers } from "./storage/deviceMarkers"
import { isInternalSaveFailure, localSaveFailureMessage } from "./storage/localSaveFailureMessage"

export interface alertData{
    type: 'error'|'normal'|'none'|'ask'|'wait'|'selectChar'
            |'input'|'toast'|'wait2'|'markdown'|'select'|'login'
            |'tos'|'risu-tos'|'cardexport'|'requestdata'|'addchar'|'hypaV2'|'selectModule'
            |'chatOptions'|'pukmakkurit'|'branches'|'progress'|'pluginconfirm'|'requestlogs'|'checkboxConfirm',
    msg: string,
    submsg?: string
    datalist?: [string, string][],
    stackTrace?: string;
    wrapCode?: boolean
    defaultValue?: string
    checkboxConfirm?: AlertCheckboxConfirmOptions
    onCheckboxConfirm?: (result: AlertCheckboxConfirmResult) => void
    onSelect?: (index: number) => void
    /** Set on `none` while the next queued dialog waits to show. */
    dialogPending?: boolean
}

export type AlertGenerationInfoStoreData = {
    genInfo: MessageGenerationInfo,
    idx: number
    message: Message
}
export const alertGenerationInfoStore = writable<AlertGenerationInfoStoreData>(null)
export const alertStore = {
    set: (d:alertData) => {
        alertStoreImported.set(d)
    }
}

/** The export reason for a file that could not be fetched or found; other export failures keep their own text. */
function exportFileFailureMessage(error: unknown): string | undefined {
    const code = error && typeof error === 'object' ? (error as { code?: unknown }).code : undefined
    const copy = language.risuNest.importDialog
    return code === 'remote-asset-unavailable' ? copy.reasonRemoteAssetUnavailable
        : code === 'asset-missing' ? copy.reasonAssetMissing : undefined
}

export function alertError(msg: string | Error) {
    console.error(msg)
    if (isInternalSaveFailure(msg)) {
        alertStoreImported.set({ 'type': 'error', 'msg': localSaveFailureMessage(msg) })
        return
    }
    const exportFileFailure = exportFileFailureMessage(msg)
    if (exportFileFailure) {
        alertStoreImported.set({ 'type': 'error', 'msg': exportFileFailure })
        return
    }
    const db = getDatabase()

    let stackTrace: string | undefined = undefined; 

    if (typeof(msg) !== 'string') {
        try{
            if (msg instanceof Error) {
                stackTrace = msg.stack
                msg = msg.message
            } else {
                msg = JSON.stringify(msg)
            }
        } catch {
            msg = `${msg}`
        }
    }

    msg = msg.trim()

    const ignoredErrors = [
        '{}'
    ]

    if(ignoredErrors.includes(msg)){
        return
    }

    let submsg = ''

    //check if it's a known error
    if(msg.includes('Failed to fetch') || msg.includes("NetworkError when attempting to fetch resource.") || msg.includes('Load failed')){
        submsg =    db.usePlainFetch ? language.errors.networkFetchPlain :
                    !isTauri ? language.errors.networkFetchWeb : language.errors.networkFetch
    }

    alertStoreImported.set({
        'type': 'error',
        'msg': msg,
        'submsg': submsg,
        'stackTrace': stackTrace
    })
}

export async function waitAlert(){
    await alertStoreImported.idle()
}

export function alertNormal(msg:string){
    alertStoreImported.set({
        'type': 'normal',
        'msg': msg
    })
}

export async function alertNormalWait(msg:string, type: 'normal' | 'error' = 'normal'){
    await alertStoreImported.open({
        'type': type,
        'msg': msg
    })
}

export async function alertAddCharacter() {
    return await alertStoreImported.open({
        'type': 'addchar',
        'msg': language.addCharacter
    })
}

export async function alertChatOptions() {
    const result = await alertStoreImported.open({
        'type': 'chatOptions',
        'msg': language.chatOptions
    })

    return parseInt(result)
}

export async function openRisuAccountLogin(open: () => void): Promise<boolean> {
    if (!(await alertRisuServiceTOS())) return false
    open()
    return true
}

export async function alertLogin(){
    if (!(await alertRisuServiceTOS())) return ""
    return await alertStoreImported.open({
        'type': 'login',
        'msg': 'login'
    })
}

export async function alertSelect(msg:string[], display?:string){
    const message = display !== undefined ? `__DISPLAY__${display}||${msg.join('||')}` : msg.join('||')
    return await alertStoreImported.open({
        'type': 'select',
        'msg': message
    })
}

export async function alertErrorWait(msg:string){
    await alertStoreImported.open({
        'type': 'wait2',
        'msg': msg
    })
}

export { alertMd } from './alertQueue'

export function doingAlert(){
    if (get(alertStoreImported).dialogPending) return true
    return get(alertStoreImported).type !== 'none' && get(alertStoreImported).type !== 'toast' && get(alertStoreImported).type !== 'wait'
}

export function alertToast(msg:string){
    alertStoreImported.set({
        'type': 'toast',
        'msg': msg
    })
}

export function alertWait(msg:string){
    alertStoreImported.set({
        'type': 'wait',
        'msg': msg
    })

}


export function alertClear(){
    alertStoreImported.clearStatus()
}

export async function alertSelectChar(){
    return await alertStoreImported.open({
        'type': 'selectChar',
        'msg': ''
    })
}

/** A labelled value shown under a confirmation's description. */
export interface AlertConfirmDetail {
    label: string
    value: string
}

export interface AlertCheckboxConfirmOptions {
    title: string
    description: string
    checkboxLabel: string
    actionLabel: string
    cancelLabel: string
    requireChecked: boolean
    details?: AlertConfirmDetail[]
}

export interface AlertCheckboxConfirmResult {
    confirmed: boolean
    checked: boolean
}

export async function alertCheckboxConfirm(options: AlertCheckboxConfirmOptions): Promise<AlertCheckboxConfirmResult> {
    // Escape and Back close the dialog without reporting a result, which counts as a cancel.
    let result: AlertCheckboxConfirmResult = { confirmed: false, checked: false }
    await alertStoreImported.open({
        type: 'checkboxConfirm', msg: options.title,
        checkboxConfirm: { ...options }, onCheckboxConfirm: (reported) => { result = reported },
    })
    return result.confirmed && options.requireChecked && !result.checked
        ? { confirmed: false, checked: false }
        : result
}

export interface AlertActionConfirmOptions {
    title: string
    description: string
    actionLabel: string
    cancelLabel: string
    details?: AlertConfirmDetail[]
}

/** The checkbox confirmation without a checkbox, for a choice whose two answers need their own labels. */
export async function alertActionConfirm(options: AlertActionConfirmOptions): Promise<boolean> {
    let confirmed = false
    await alertStoreImported.open({
        type: 'checkboxConfirm', msg: options.title,
        checkboxConfirm: { ...options, checkboxLabel: '', requireChecked: false },
        onCheckboxConfirm: (reported) => { confirmed = reported.confirmed },
    })
    return confirmed
}

export async function alertConfirm(msg:string){

    const result = await alertStoreImported.open({
        'type': 'ask',
        'msg': msg
    })

    return result === 'yes'
}

export async function alertPluginConfirm(msg:string){

    const result = await alertStoreImported.open({
        'type': 'pluginconfirm',
        'msg': msg
    })

    return result === 'yes'
}

export async function alertCardExport(type:string = ''){

    const result = await alertStoreImported.open({
        'type': 'cardexport',
        'msg': '',
        'submsg': type
    })

    return JSON.parse(result) as {
        type: string,
        type2: string,
    }
}

// RisuNest's own terms and the RisuAI service terms never stand in for each
// other, so each keeps its own key. Callers also treat a refusal differently:
// refusing ours leaves the app unusable (bootstrap), while refusing the upstream
// service terms cancels only that one action.
export async function alertTOS(){
    return askLegalAcceptance('tos', 'risunest_tos_v1')
}

export async function alertRisuServiceTOS(){
    return askLegalAcceptance('risu-tos', 'risu_service_tos_v1')
}

async function askLegalAcceptance(type: 'tos'|'risu-tos', acceptanceKey: string){

    const markers = getDeviceMarkers()
    if(markers.getItem(acceptanceKey) === 'true'){
        return true
    }

    const result = await alertStoreImported.open({
        'type': type,
        'msg': type
    })

    if(result === 'yes'){
        markers.setItem(acceptanceKey, 'true')
        await markers.flush()
        return true
    }

    return false
}

export async function alertInput(msg:string, datalist?:[string, string][], defaultValue?:string) {

    return await alertStoreImported.open({
        'type': 'input',
        'msg': msg,
        'datalist': datalist ?? [],
        'defaultValue': defaultValue ?? ''
    })
}

export async function alertModuleSelect(){

    return await alertStoreImported.open({
        'type': 'selectModule',
        'msg': ''
    })
}

export function alertRequestData(info:AlertGenerationInfoStoreData){
    alertGenerationInfoStore.set(info)
    alertStoreImported.set({
        'type': 'requestdata',
        'msg': info.genInfo.generationId ?? 'none'
    })
}

export function showHypaV2Alert(){
    alertStoreImported.set({
        'type': 'hypaV2',
        'msg': ""
    })
}

export function alertRequestLogs(){
    alertStoreImported.set({
        'type': 'requestlogs',
        'msg': ''
    })
}
