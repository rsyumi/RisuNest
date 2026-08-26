import { get } from "svelte/store";
import { CharEmotion, selectedCharID } from "../stores.svelte";
import { type character, type customscript, type Database, type groupChat, type loreBook, getDatabase, getCurrentCharacter, getCurrentChat } from "../storage/database.svelte";
import { downloadFile } from "../globalApi.svelte";
import { alertError, alertNormal } from "../alert";
import { language } from "src/lang";
import { selectSingleFile } from "../util";
import { assetRegex, type CbsConditions, risuChatParser as risuChatParserOrg, type simpleCharacterArgument } from "../parser/parser.svelte";
import { getModuleAssets, getModuleRegexScripts, type RisuModule } from "./modules";
import { HypaProcesser } from "./memory/hypamemory";
import { runLuaEditTrigger } from "./scriptings";
import { pluginV2 } from "../plugins/plugins.svelte";
import { runTrigger } from "./triggers";
import { ByteBudgetLru } from "../util/byteBudgetLru";
import { canExecuteRegexPlanInWorker, executeRegexPlanSync, getRegexExecutionPlan, type RegexExecutionPlanEntry, type RegexExecutionResult } from "./regexExecutionPlan";
import { RegexExecutionTimeoutError, getSharedRegexWorkerClient, isRegexWorkerAvailable } from "./regexWorkerClient";
import { getRuntimePerformanceBudgets, subscribeRuntimePerformanceProfile } from "../runtimePerformanceProfile";

export type ScriptMode = 'editinput'|'editoutput'|'editprocess'|'editdisplay'

export interface ProcessScriptOptions {
    cache?: 'normal' | 'bypass'
    signal?: AbortSignal
    /** true forces the Worker path, false opts out, undefined offloads whenever a Worker is available. */
    regexWorker?: boolean
    captureContext?: ProcessScriptCaptureContext
    projectedChatID?: number
}

export interface ProcessScriptCaptureContext {
    presetRegex: readonly customscript[]
    moduleRegexScripts: readonly customscript[]
    moduleAssets: readonly (readonly [string, string, string])[]
    dynamicAssets: boolean
    dynamicAssetsEditDisplay: boolean
    parserContext: {
        database: Database
        character: character | groupChat
        chara?: character | groupChat | string
        userName: string
        personaPrompt: string
        modules: RisuModule[]
        moduleLorebooks: loreBook[]
        selectedCharID: number
        chatVariables: Record<string, string>
        globalChatVariables: Record<string, string>
        currentTime: number
        triggerId?: string
        historyOffset?: number
    }
}

export async function processScript(char:character|groupChat, data:string, mode:ScriptMode, cbsConditions:CbsConditions = {}){
    return (await processScriptFull(char, data, mode, -1, cbsConditions)).data
}

export function exportRegex(s?:customscript[]){
    let db = getDatabase()
    const script = s ?? db.globalscript
    const data = Buffer.from(JSON.stringify({
        type: 'regex',
        data: script
    }), 'utf-8')
    downloadFile(`regexscript_export.json`,data)
    alertNormal(language.successExport)
}

export async function importRegex(o?:customscript[]):Promise<customscript[]>{
    o = o ?? []
    const filedata = (await selectSingleFile(['json'])).data
    if(!filedata){
        return o
    }
    let db = getDatabase()
    try {
        const imported= JSON.parse(Buffer.from(filedata).toString('utf-8'))
        if(imported.type === 'regex' && imported.data){
            const datas:customscript[] = imported.data
            const script = o
            for(const data of datas){
                script.push(data)
            }
            return o
        }
        else{
            alertError("File invaid or corrupted")
        }

    } catch (error) {
        alertError(error)
    }
    return o
}

let bestMatchCache = new Map<string, string>()
let processScriptCache = createScriptCache()

function createScriptCache() {
    const budgets = getRuntimePerformanceBudgets()
    return new ByteBudgetLru<string, string>(
        budgets.scriptResultCacheBytes,
        (key, result) => 2 * (key.length + result.length),
        budgets.scriptResultCacheEntries,
    )
}

subscribeRuntimePerformanceProfile(() => {
    processScriptCache = createScriptCache()
})

function generateScriptCacheKey(
    scripts: customscript[],
    data: string,
    mode: ScriptMode,
    chatID = -1,
    cbsConditions: CbsConditions = {},
    parseCbs = (value: string) => risuChatParser(value, { chatID, cbsConditions }),
) {
    let hash = data + '|||' + mode + '|||';
    for (const script of scripts) {
        if(script.type !== mode){
            continue
        }
        hash += `${script.flag?.includes('<cbs>') ? parseCbs(script.in) : script.in}|||${script.out}${chatID}|||${script.flag ?? ''}|||${script.ableFlag ? 1 : 0}`;
    }
    return hash;
}

function cacheScript(hash:string, result:string){
    processScriptCache.set(hash, result)
}

function getScriptCache(hash:string){
    return processScriptCache.get(hash)
}

export function resetScriptCache(){
    processScriptCache = createScriptCache()
}

export async function processScriptFull(char:character|groupChat|simpleCharacterArgument, data:string, mode:ScriptMode, chatID = -1, cbsConditions:CbsConditions = {}, options:ProcessScriptOptions = {}){
    const captureContext = options.captureContext
    let db = captureContext?.parserContext.database ?? getDatabase()
    const parseCbs = (value: string) => risuChatParser(value, captureContext ? {
        chatID,
        projectedChatID: options.projectedChatID,
        historyOffset: captureContext.parserContext.historyOffset,
        cbsConditions,
        db: captureContext.parserContext.database,
        chara: captureContext.parserContext.chara ?? captureContext.parserContext.character,
        userName: captureContext.parserContext.userName,
        personaPrompt: captureContext.parserContext.personaPrompt,
        modules: captureContext.parserContext.modules,
        moduleLorebooks: captureContext.parserContext.moduleLorebooks,
        selectedCharID: captureContext.parserContext.selectedCharID,
        chatVariables: captureContext.parserContext.chatVariables,
        globalChatVariables: captureContext.parserContext.globalChatVariables,
        currentTime: captureContext.parserContext.currentTime,
        triggerId: captureContext.parserContext.triggerId,
        role: cbsConditions.chatRole,
    } : { chatID, cbsConditions })
    let emoChanged = false
    if (!captureContext) {
        data = await runLuaEditTrigger(char, mode, data, { index:chatID })
    }

    if(mode === 'editdisplay' && !captureContext){
        const currentChar = getCurrentCharacter()
        if(currentChar.type !== 'group'){
            try{
                const perf = performance.now()
                const d = await runTrigger(currentChar, 'display', {
                    chat: getCurrentChat(),
                    displayMode: true,
                    displayData: data
                })
    
                data = d?.displayData ?? data
                console.log('Trigger time', performance.now() - perf)
            }
            catch(e){
                console.error(e)
            }
        }
    }

    if(!captureContext && pluginV2[mode].size > 0){
        for(const plugin of pluginV2[mode]){
            const res = await plugin(data)
            if(res !== null && res !== undefined){
                data = res
            }
        }
    }

    data = parseCbs(data)
    const scripts = [
        ...(captureContext?.presetRegex ?? db.presetRegex ?? []),
        ...char.customscript,
        ...(captureContext?.moduleRegexScripts ?? getModuleRegexScripts()),
    ]
    const useResultCache = options.cache !== 'bypass'
    const hash = useResultCache ? generateScriptCacheKey(scripts, data, mode, chatID, cbsConditions, parseCbs) : undefined
    if(!useResultCache){
        for(const script of scripts){
            if(script.type === mode && script.flag?.includes('<cbs>')){
                parseCbs(script.in)
            }
        }
    }
    if(hash !== undefined){
        const cached = getScriptCache(hash)
        if(cached !== undefined){
            return {data: cached, emoChanged: false}
        }
    }
    
    if(scripts.length === 0){
        if(hash !== undefined){
            cacheScript(hash, data)
        }
        return {data, emoChanged}
    }

    const plan = getRegexExecutionPlan(scripts, mode)
    const parse = parseCbs

    function executeScript(entry:RegexExecutionPlanEntry){
        const script = entry.script
        
        if(script.in === ''){
            return
        }

        const outScript = entry.replacement
        const flag = entry.flags
        let reg: RegExp
        if(entry.dynamicPattern){
            reg = new RegExp(parse(entry.pattern), flag)
        }
        else{
            if(entry.compileError !== undefined){
                throw entry.compileError
            }
            if(entry.compiledRegex === undefined){
                throw new Error('Regex execution plan entry was not compiled')
            }
            reg = entry.compiledRegex
        }
        reg.lastIndex = 0

            if(outScript.startsWith('@@') || entry.actions.length > 0){
                if(reg.test(data)){
                    if(outScript.startsWith('@@emo ')){
                        const emoName = script.out.substring(6).trim()
                        let charemotions = get(CharEmotion)
                        let tempEmotion = charemotions[char.chaId]
                        if(!tempEmotion){
                            tempEmotion = []
                        }
                        if(tempEmotion.length > 4){
                            tempEmotion.splice(0, 1)
                        }
                        if(char.type !== 'simple'){
                            for(const emo of char.emotionImages){
                                if(emo[0] === emoName){
                                    const emos:[string, string,number] = [emo[0], emo[1], Date.now()]
                                    tempEmotion.push(emos)
                                    charemotions[char.chaId] = tempEmotion
                                    CharEmotion.set(charemotions)
                                    emoChanged = true
                                    break
                                }
                            }
                        }
                    }
                    else if((outScript.startsWith('@@inject') || entry.actions.includes('inject')) && chatID !== -1){
                        if (!captureContext) {
                            const selchar = db.characters[get(selectedCharID)]
                            selchar.chats[selchar.chatPage].message[chatID].data = data
                        }
                        reg.lastIndex = 0
                        data = data.replace(reg, "")
                    }
                    else if(
                        outScript.startsWith('@@move_top') || outScript.startsWith('@@move_bottom') ||
                        entry.actions.includes('move_top') || entry.actions.includes('move_bottom')
                    ){
                        const isGlobal = flag.includes('g')
                        reg.lastIndex = 0
                        const matchAll = isGlobal ? data.matchAll(reg) : [data.match(reg)]
                        reg.lastIndex = 0
                        data = data.replace(reg, "")
                        for(const matched of matchAll){
                            if(matched){
                                const inData = matched[0]
                                let out = outScript.replace('@@move_top ', '').replace('@@move_bottom ', '')
                                    .replace(/(?<!\$)\$[0-9]+/g, (v)=>{
                                        const index = parseInt(v.substring(1))
                                        if(index < matched.length){
                                            return matched[index]
                                        }
                                        return v
                                    })
                                    .replace(/\$\&/g, inData)
                                    .replace(/(?<!\$)\$<([^>]+)>/g, (v) => {
                                        const groupName = parseInt(v.substring(2, v.length - 1))
                                        if(matched.groups && matched.groups[groupName]){
                                            return matched.groups[groupName]
                                        }
                                        return v
                                    })
                                if(outScript.startsWith('@@move_top') || entry.actions.includes('move_top')){
                                    data = out + '\n' +data
                                }
                                else{
                                    data = data + '\n' + out
                                }
                            }
                        }
                    }
                    else{
                        reg.lastIndex = 0
                        data = parse(data.replace(reg, outScript))
                    }
                }
                else{
                    if((outScript.startsWith('@@repeat_back') || entry.actions.includes('repeat_back'))  && chatID !== -1){
                        const v = outScript.split(' ', 2)[1]
                        const selectedIndex = captureContext?.parserContext.selectedCharID ?? get(selectedCharID)
                        const selchar = db.characters[selectedIndex]
                        const chat = selchar.chats[selchar.chatPage]
                        let lastChat = chat.fmIndex === -1 ? selchar.firstMessage : selchar.alternateGreetings[chat.fmIndex]
                        const historyChatID = options.projectedChatID ?? chatID
                        let pointer = historyChatID - 1
                        while(pointer >= 0){
                            if(chat.message[pointer].role === chat.message[historyChatID].role){
                                lastChat = chat.message[pointer].data
                                break
                            }
                            pointer--
                        }

                        reg.lastIndex = 0
                        const r = lastChat.match(reg)
                        if(!v){
                            data = data + r[0]
                        }
                        else if(r[0]){
                            switch(v){
                                case 'end':
                                    data = data + r[0]
                                    break
                                case 'start':
                                    data = r[0] + data
                                    break
                                case 'end_nl':
                                    data = data + "\n" + r[0]
                                    break
                                case 'start_nl':
                                    data = r[0] + "\n" + data
                                    break
                            }

                        }                        
                    }
                }
            }
            else{
                data = parse(data.replace(reg, outScript))
            }
    }

    if(plan.requiresHostExecution){
        for (const entry of plan.entries){
            try {
                executeScript(entry)
            } catch (error) {
                console.error(error)
            }
        }
    }
    else if((options.regexWorker ?? isRegexWorkerAvailable()) && mode === 'editoutput' && canExecuteRegexPlanInWorker(plan, data)){
        let result: RegexExecutionResult
        try {
            result = await getSharedRegexWorkerClient().execute(plan, data, { signal: options.signal })
        } catch (error) {
            // A pathological ruleset must not be retried on the UI thread, and a cancelled
            // generation must stay cancelled. Anything else means the Worker is unusable here.
            if(error instanceof RegexExecutionTimeoutError || options.signal?.aborted){
                throw error
            }
            console.error(error)
            result = executeRegexPlanSync(plan, data, parse)
        }
        data = result.data
        for(const error of result.errors){
            console.error(error.error)
        }
    }
    else{
        data = executeRegexPlanSync(plan, data, parse).data
    }

    

    const dynamicAssets = captureContext?.dynamicAssets ?? db.dynamicAssets
    const dynamicAssetsEditDisplay = captureContext?.dynamicAssetsEditDisplay ?? db.dynamicAssetsEditDisplay
    if(dynamicAssets && (char.type === 'simple' || char.type === 'character') && char.additionalAssets && char.additionalAssets.length > 0){
        if((!dynamicAssetsEditDisplay && mode === 'editdisplay')
            || mode === 'editinput' || mode === 'editprocess'){
            if(hash !== undefined){
                cacheScript(hash, data)
            }
            return {data, emoChanged}
        }
        const assetNames = char.additionalAssets.map((v) => v[0])

        const moduleAssets = captureContext?.moduleAssets ?? getModuleAssets()
        if(moduleAssets.length > 0){
            for(const asset of moduleAssets){
                assetNames.push(asset[0])
            }
        }

        const processer = new HypaProcesser()
        await processer.addText(assetNames)
        const matches = data.matchAll(assetRegex)

        for(const match of matches){
            const type = match[1]
            const assetName = match[2]
            const cacheKey = char.chaId + '::' + assetName
            if(type !== 'emotion' && type !== 'source'){
                if(bestMatchCache.has(cacheKey)){
                    data = data.replaceAll(match[0], `{{${type}::${bestMatchCache.get(cacheKey)}}}`)
                }
                else if(!assetNames.includes(assetName)){
                    const searched = await processer.similaritySearch(assetName)
                    const bestMatch = searched[0]
                    if(bestMatch){
                        data = data.replaceAll(match[0], `{{${type}::${bestMatch}}}`)
                        bestMatchCache.set(cacheKey, bestMatch)
                    }
                }
            }
        }
    }

    if(hash !== undefined){
        cacheScript(hash, data)
    }

    return {data, emoChanged}
}


const rgx = /(?:{{|<)(.+?)(?:}}|>)/gm
export const risuChatParser = risuChatParserOrg
