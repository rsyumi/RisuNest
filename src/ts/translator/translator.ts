import { get } from "svelte/store"
import { parseChatML } from "../parser/chatML";
import { getDatabase, type character, type customscript, type Database, type groupChat } from "../storage/database.svelte"
import {
    defaultTranslatorPrompt,
    getCurrentTranslatorPresetFromState,
    type TranslatorPreset,
} from "./presets";
import { globalFetch } from "../globalApi.svelte"
import { isTauri, isNodeServer } from "src/ts/platform"
import { alertError } from "../alert"
import { requestChatData } from "../process/request/request"
import { doingChat, type OpenAIChat } from "../process/index.svelte"
import { applyMarkdownToNode, risuChatParser, type CbsConditions, type simpleCharacterArgument } from "../parser/parser.svelte"
import { selectedCharID } from "../stores.svelte"
import { getModuleRegexScripts } from "../process/modules"
import { getNodetextToSentence, sleep } from "../util"
import { processScriptFull, type ProcessScriptCaptureContext } from "../process/scripts"
import localforage from "localforage"
import sendSound from '../../etc/send.mp3'

let cache={
    origin: [''],
    trans: ['']
}

let bergamotTranslate: (text: string, from: string, to: string, html?: boolean) => Promise<string>|null = null

export const LLMCacheStorage = localforage.createInstance({
    name: "LLMTranslateCache"
})

let waitTrans = 0

export interface TranslateHTMLContext {
    scriptContext: ProcessScriptCaptureContext
    projectedChatID?: number
    chara?: character | groupChat | string
    cbsConditions?: CbsConditions
}

export function getCurrentTranslatorPreset(database: Database = getDatabase()): TranslatorPreset {
    return getCurrentTranslatorPresetFromState(database)
}

export async function translate(text:string, reverse:boolean, captureContext?: TranslateHTMLContext) {
    let db = captureContext?.scriptContext.parserContext.database ?? getDatabase()
    if(!captureContext){
        if(!reverse){
            const ind = cache.origin.indexOf(text)
            if(ind !== -1){
                return cache.trans[ind]
            }
        }
        else{
            const ind = cache.trans.indexOf(text)
            if(ind !== -1){
                return cache.origin[ind]
            }
        }
    }

    return runTranslator(
        text,
        reverse,
        db.translator,
        db.aiModel.startsWith('novellist') ? 'ja' : 'en',
        undefined,
        captureContext,
    )
}

export async function runTranslator(text:string, reverse:boolean, from:string,target:string, exarg?:{translatorNote?:string}, captureContext?: TranslateHTMLContext) {
    const arg = {

        from: reverse ? from : target,

        to: reverse ? target : from,

        host: 'translate.googleapis.com',

        translatorNote: exarg?.translatorNote
    }
    const texts = text.split('\n')
    let chunks:[string,boolean][] = [['', true]]

    for(let i = 0; i < texts.length; i++){
        if( texts[i].startsWith('{{img')
            || texts[i].startsWith('{{raw')
            || texts[i].startsWith('{{video')
            || texts[i].startsWith('{{audio')
            && texts[i].endsWith('}}')
            || texts[i].length === 0){
            chunks.push([texts[i], false])
            chunks.push(["", true])
        }
        else{
            chunks[chunks.length-1][0] += texts[i]
        }
    }

    let fullResult:string[] = []

    for(const chunk of chunks){
        if(chunk[1]){
            const trimed = chunk[0].trim();
            if(trimed.length === 0){
                fullResult.push(chunk[0])
                continue
            }
            const result = await translateMain(trimed, arg, captureContext);

            if(result.startsWith('ERR::')){
                alertError(result)
                return text
            }


            fullResult.push(result.trim())
        }
        else{
            fullResult.push(chunk[0])
        }
    }

    const result = fullResult.join("\n").trim()

    if(!captureContext){
        cache.origin.push(reverse ? result : text)
        cache.trans.push(reverse ? text : result)
    }


    return result

}

async function translateMain(text:string, arg:{from:string, to:string, host:string, translatorNote?:string}, captureContext?: TranslateHTMLContext){
    let db = captureContext?.scriptContext.parserContext.database ?? getDatabase()
    if(db.translatorType === 'llm'){
        const tr = arg.to || 'en'
        return translateLLM(text, {to: tr, from: arg.from, translatorNote: arg.translatorNote}, captureContext)
    }
    if(db.translatorType === 'deepl'){
        const body = {
            text: [text],
            target_lang: arg.to.toLocaleUpperCase(),
        }
        let url = db.deeplOptions.freeApi ? "https://api-free.deepl.com/v2/translate" : "https://api.deepl.com/v2/translate"
        const f = await globalFetch(url, {
            headers: {
                "Authorization": "DeepL-Auth-Key " + db.deeplOptions.key,
                "Content-Type": "application/json"
            },
            body: body
        })

        if(!f.ok){
            return 'ERR::DeepL API Error' + (await f.data)
        }
        return f.data.translations[0].text

    }
    if(db.translatorType === 'deeplX'){
        if(!db.noWaitForTranslate){
            if(waitTrans - Date.now() > 0){
                const waitTime = waitTrans - Date.now()
                waitTrans = Date.now() + 3000
                await sleep(waitTime)
            }
        }

        let url = db.deeplXOptions.url ?? 'http://localhost:1188'

        if(url.endsWith('/')){
            url = url.slice(0, -1)
        }

        if(!url.endsWith('/translate')){
            url += '/translate'
        }

        let headers = { "Content-Type": "application/json" }

        const body = {text: text, target_lang: arg.to.toLocaleUpperCase(), source_lang: arg.from.toLocaleUpperCase()}

    
        if(db.deeplXOptions.token.trim() !== '') { headers["Authorization"] = "Bearer " + db.deeplXOptions.token}
        
        //Since the DeepLX API is non-CORS restricted, we can use the plain fetch function
        const f = await globalFetch(url, { method: "POST", headers: headers, body: body, plainFetchForce:true })

        if(!f.ok){ return 'ERR::DeepLX API Error' + (await f.data) }

        return f.data.data;
    }
    if(db.translatorType == "bergamot") {
        if(!bergamotTranslate){
            const bergamotTranslator = await import('./bergamotTranslator')
            bergamotTranslate = bergamotTranslator.bergamotTranslate
        }

        return bergamotTranslate(text, arg.from, arg.to, false);
    }
    if(db.useExperimentalGoogleTranslator){

        const hqAvailable = isTauri || isNodeServer || userScriptFetch

        if(hqAvailable){
            try {
                const ua = navigator.userAgent
                const d = await globalFetch(`https://translate.google.com/m?tl=${arg.to}&sl=${arg.from}&q=${encodeURIComponent(text)}`, {
                    headers: {
                        "User-Agent": ua,
                        "Accept": "*/*",
                    },
                    method: "GET",
                })
                const parser = new DOMParser()
                const dom = parser.parseFromString(d.data, 'text/html')
                const result = dom.querySelector('.result-container')?.textContent?.trim()
                if(result){
                    return result
                }
            } catch (error) {
                
            }
        }
    }


    const url = `https://${arg.host}/translate_a/single?client=gtx&dt=t&sl=${db.translatorInputLanguage}&tl=${arg.to}&q=` + encodeURIComponent(text)



    const f = await fetch(url, {

        method: "GET",

    })

    const res = await f.json()

    

    if(typeof(res) === 'string'){

        return res as unknown as string

    }

    if((!res[0]) || res[0].length === 0){
        return text
    }

    const result = (res[0].map((s) => s[0]).filter(Boolean).join('') as string).replace(/\* ([^*]+)\*/g, '*$1*').replace(/\*([^*]+) \*/g, '*$1*');
    return result
}

export async function translateVox(text:string) {    
    return jaTrans(text)
}


async function jaTrans(text:string) {
    return await runTranslator(text, true, 'en','ja')
}

export function isExpTranslator(database: Database = getDatabase()){
    const db = database
    return db.translatorType === 'llm' || db.translatorType === 'deepl' || db.translatorType === 'deeplX'
}

export async function translateHTML(
    html: string,
    reverse:boolean,
    charArg:simpleCharacterArgument|character|groupChat|string|null = '',
    chatID:number,
    regenerate = false,
    captureContext?: TranslateHTMLContext,
    signal?: AbortSignal,
): Promise<string> {
    signal?.throwIfAborted()
    if (!html) {
        return html
    }

    const db = captureContext?.scriptContext.parserContext.database ?? getDatabase()
    let alwaysExistChar: character | groupChat | simpleCharacterArgument;
    if(charArg){
        if(typeof(charArg) === 'string'){
            const charId = captureContext?.scriptContext.parserContext.selectedCharID ?? get(selectedCharID)
            alwaysExistChar = db.characters[charId]
        }
        else{
            alwaysExistChar=charArg
        }
    } else if(captureContext) {
        alwaysExistChar = captureContext.scriptContext.parserContext.character
    } else {
        alwaysExistChar = {
            type: 'simple',
            customscript: [],
            virtualscript: null,
            emotionImages: [],
            chaId: 'simple'
        }
    }
    let DoingChat = captureContext ? false : get(doingChat)
    if(DoingChat){
        if(isExpTranslator(db)){
            if(!(db.translatorType === 'llm' && await getLLMCache(html) !== null)){
                return html
            }
        }
    }
    if(db.translatorType === 'llm'){
        const tr = db.translator || 'en'
        const from = db.translatorInputLanguage
        const r = await translateLLM(html, {to: tr, from: from, regenerate}, captureContext)
        signal?.throwIfAborted()
        if(db.playMessageOnTranslateEnd){
            const audio = new Audio(sendSound);
            audio.play().catch(() => {});
        }

        return applyEdittransRegex(r, charArg, alwaysExistChar, chatID, captureContext)
    }
    if(db.translatorType == "bergamot" && db.htmlTranslation) {
        const from = db.aiModel.startsWith('novellist') ? 'ja' : 'en'
        const to = db.translator || 'en'

        if(!bergamotTranslate){
            const bergamotTranslator = await import('./bergamotTranslator')
            bergamotTranslate = bergamotTranslator.bergamotTranslate
        }
 
        const translated = await bergamotTranslate(html, from, to, true)
        signal?.throwIfAborted()
        return applyEdittransRegex(translated, charArg, alwaysExistChar, chatID, captureContext)
    }
    const dom = new DOMParser().parseFromString(html, 'text/html');
    console.log(html)

    let promises: Promise<void>[] = [];
    let translationChunks: {
        chunks: string[],
        resolvers: ((text:string) => void)[]
    }[] = [{
        chunks: [],
        resolvers: []
    }]
    

    async function translateTranslationChunks(force:boolean = false, additionalChunkLength = 0){
        if(translationChunks.length === 0 || !needSuperChunkedTranslate(db)){
            return
        }

        const currentChunk = translationChunks[translationChunks.length-1]
        const text: string = currentChunk.chunks.join('\n■\n')

        if(!force && text.length + additionalChunkLength < 5000){
            return
        }

        translationChunks.push({
            chunks: [],
            resolvers: []
        })

        if(!text){
            return
        }

        const translated = await translate(text, reverse, captureContext)

        const split = translated.split('■')

        console.log(split.length, currentChunk.chunks.length)

        if(split.length !== currentChunk.chunks.length){
            //try translating one by one
            for(let i = 0; i < currentChunk.chunks.length; i++){
                currentChunk.resolvers[i](
                    await translate(currentChunk.chunks[i]
                , reverse, captureContext))
            }
        }
        
        for(let i = 0; i < split.length; i++){
            console.log(split[i])
            currentChunk.resolvers[i](split[i])
        }


    }

    async function translateNodeText(node:Node, reprocessDisplayScript:boolean = false) {
        if(node.textContent.trim().length !== 0){
            if(needSuperChunkedTranslate(db)){
                const prm = new Promise<string>((resolve) => {
                    translateTranslationChunks(false, node.textContent.length)
                    translationChunks[translationChunks.length-1].resolvers.push(resolve)
                    translationChunks[translationChunks.length-1].chunks.push(node.textContent)
                })
    
                node.textContent = await prm
                return
            }

            const translateChunks = (node.textContent || '').split(/\n\n+/g);
            let translatedChunksPromises: Promise<string>[] = [];
            for (const chunk of translateChunks) {
                const translatedPromise = translate(chunk, reverse, captureContext);
                translatedChunksPromises.push(translatedPromise);
            }

            const translatedChunks = await Promise.all(translatedChunksPromises);
            signal?.throwIfAborted()
            let translated = translatedChunks.join("\n\n");
            if (!reprocessDisplayScript) {
                node.textContent = translated;
                return;
            }
            
            const { data: processedTranslated } = await processScriptFull(
                alwaysExistChar,
                translated,
                "editdisplay",
                chatID,
                captureContext?.cbsConditions,
                {
                    captureContext: captureContext?.scriptContext,
                    projectedChatID: captureContext?.projectedChatID,
                    cache: captureContext ? 'bypass' : 'normal',
                    signal,
                },
            );
            // If the translation is the same, don't replace the node
            if (translated == processedTranslated) {
                node.textContent = processedTranslated;
                applyMarkdownToNode(node)
                return;
            }

            // Replace the old node with the new one
            const newNode = document.createElement(
                node.nodeType === Node.TEXT_NODE ? "span" : node.nodeName
            );
            newNode.innerHTML = processedTranslated;
            node.parentNode.replaceChild(newNode, node);
            applyMarkdownToNode(newNode);
        }
    }

    // Recursive function to translate all text nodes
    async function translateNode(node: Node, parent?: Node): Promise<void> {
        if (node.nodeType === Node.TEXT_NODE) {
            // Translate the text content of the node
            if(node.textContent && parent){
                const parentName = parent.nodeName.toLowerCase();
                if(parentName === 'script' || parentName === 'style'){
                    return
                }
                if(promises.length > 10){
                    await Promise.all(promises)
                    promises = []
                }
                promises.push(translateNodeText(node))
            }
        } else if(node.nodeType === Node.ELEMENT_NODE) {
            // Translate child nodes
            //skip if it's a script or style tag
            if(node.nodeName.toLowerCase() === 'script' || node.nodeName.toLowerCase() === 'style'){
                return
            }
            // combineTranslation feature
            if (
                db.combineTranslation &&
                node.nodeName.toLowerCase() === "p" &&
                node instanceof HTMLElement
            ) {
                const children = Array.from(node.childNodes);
                const blacklist = ["img", "iframe", "script", "style", "div", "button", "audio", "video"];
                const hasBlacklistChild = children.some((child) =>
                    blacklist.includes(child.nodeName.toLowerCase())
                );
                if (!hasBlacklistChild && (node as Element)?.getAttribute('translate') !== 'no'){
                    const text = getNodetextToSentence(node);
                    const sentences = text.split("\n");
                    if (sentences.length > 1) {
                        // Multiple sentences seperated by <br> tags
                        // reconstruct the p tag
                        node.innerHTML = "";
                        for (const sentence of sentences) {
                            const newNode = document.createElement("span");
                            newNode.textContent = sentence;
                            node.appendChild(newNode);
                            await translateNodeText(newNode, true);
                            node.appendChild(document.createElement("br"));
                        }
                    } else {
                        // Single sentence
                        node.innerHTML = sentences[0];
                        await translateNodeText(node, true);
                    }
                    return;
                }
            }

            for (const child of Array.from(node.childNodes)) {
                if(node.nodeType === Node.ELEMENT_NODE && (node as Element)?.getAttribute('translate') === 'no'){
                    continue
                }
                await translateNode(child, node);
            }
        }
    }
    

    // Start translation from the body element
    await translateNode(dom.body);

    await translateTranslationChunks(true, 0)

    await Promise.all(promises)
    signal?.throwIfAborted()
    // Serialize the DOM back to HTML
    const serializer = new XMLSerializer();
    let translatedHTML = serializer.serializeToString(dom);
    // Remove the outer <html|body|head> tags
    translatedHTML = translatedHTML.replace(/<\/?(html|body|head)[^>]*>/g, '');

    translatedHTML = applyEdittransRegex(translatedHTML, charArg, alwaysExistChar, chatID, captureContext);

    // console.log(html)
    // console.log(translatedHTML)
    // Return the translated HTML, excluding the outer <body> tags if needed
    return translatedHTML
}

function needSuperChunkedTranslate(database: Database = getDatabase()){
    return database.translatorType === 'deeplX'
}

// Deferred inlay slot markers embed a per-render counter, so two parses of
// the same message can differ only by slot numbers. Strip them so cache keys
// stay stable across renders and match key computations that parse without
// a marker registry (e.g. translation edit in Chat.svelte).
const inlaySlotMarkerRegex = /\sdata-risu-inlay-slot="[0-9a-z]+"/g
function normalizeLLMCacheKey(key:string):string{
    return key.replace(inlaySlotMarkerRegex, '')
}

async function translateLLM(text:string, arg:{to:string, from:string, regenerate?:boolean,translatorNote?:string}, captureContext?: TranslateHTMLContext):Promise<string>{
    if(!arg.regenerate){
        const cacheMatch = await LLMCacheStorage.getItem(normalizeLLMCacheKey(text))
        if(cacheMatch !== null){
            return cacheMatch as string
        }
    }
    const styleDecodeRegex = /\<risu-style\>(.+?)\<\/risu-style\>/gms
    let styleDecodes:string[] = []
    text = text.replace(styleDecodeRegex, (match, p1) => {
        styleDecodes.push(p1)
        return `<style-data style-index="${styleDecodes.length-1}"></style-data>`
    })

    const db = captureContext?.scriptContext.parserContext.database ?? getDatabase()
    const charIndex = captureContext?.scriptContext.parserContext.selectedCharID ?? get(selectedCharID)
    const currentChar = db.characters[charIndex]
    let translatorNote = ""
    console.log(arg.translatorNote)
    if(arg.translatorNote){
        translatorNote = arg.translatorNote
    }
    else if (currentChar?.type === "character") {
        translatorNote = currentChar.translatorNote ?? ""
    } else {
        translatorNote = ""
    }
    console.log(translatorNote)

    let formated:OpenAIChat[] = []
    const preset = captureContext
        ? getCurrentTranslatorPreset({
            ...db,
            translatorPresets: db.translatorPresets?.map((preset) => ({ ...preset })),
        })
        : getCurrentTranslatorPreset(db)
    let prompt = preset.prompt || defaultTranslatorPrompt
    let parsedPrompt = parseChatML(prompt.replaceAll('{{slot::from}}', arg.from).replaceAll('{{slot}}', arg.to).replaceAll('{{solt::content}}', text).replaceAll('{{slot::content}}', text).replaceAll('{{slot::tnote}}', translatorNote))
    if(parsedPrompt){
        formated = parsedPrompt
    }
    else{
        prompt = prompt.replaceAll('{{slot}}', arg.to).replaceAll('{{slot::tnote}}', translatorNote).replaceAll('{{slot::from}}', arg.from)
        formated = [
            {
                'role': 'system',
                'content': prompt
            },
            {
                'role': 'user',
                'content': text
            }
        ]
    }
    const rq = await requestChatData({
        formated,
        bias: {},
        useStreaming: false,
        noMultiGen: true,
        maxTokens: preset.maxResponse,
    }, 'translate')

    if(rq.type === 'fail'){
        alertError(rq.result)
        return text
    }
    if(rq.type === 'streaming' || rq.type === 'multiline'){
        alertError('Unexpected response type')
        return text
    }
    const result = rq.result.replace(/<style-data style-index="(\d+)" ?\/?>/g, (match, p1) => {
        return styleDecodes[parseInt(p1)] ?? ''
    }).replace(/<\/style-data>/g, '')
    await LLMCacheStorage.setItem(normalizeLLMCacheKey(text), result)
    return result
}

export async function getLLMCache(text:string):Promise<string | null>{
    return await LLMCacheStorage.getItem(normalizeLLMCacheKey(text))
}

export async function searchLLMCache(partialKey:string):Promise<{key: string, value: string}[]>{
    const results:{key: string, value: string}[] = []
    await LLMCacheStorage.iterate<string, void>((value, key) => {
        if(key.includes(partialKey)){
            results.push({key, value})
        }
    })
    return results
}

export async function setLLMCache(key:string, value:string):Promise<void>{
    await LLMCacheStorage.setItem(normalizeLLMCacheKey(key), value)
}

export async function exportLLMCacheAsJSON():Promise<Record<string, string>>{
    const result:Record<string, string> = {}
    await LLMCacheStorage.iterate<string, void>((value, key) => {
        result[key] = value
    })
    return result
}

export async function importLLMCacheFromJSON(data:Record<string, string>):Promise<{count: number, failed: number}>{
    let count = 0
    let failed = 0
    for(const [key, value] of Object.entries(data)){
        try{
            await LLMCacheStorage.setItem(normalizeLLMCacheKey(key), value)
            count++
        }catch{
            failed++
        }
    }
    return {count, failed}
}

export async function clearLLMCache():Promise<void>{
    await LLMCacheStorage.clear()
}


interface pEdittransScript {
    script: customscript
    flag: string
    order: number
    actions: string[]
}

export function applyEdittransRegex(
      text: string,
      charArg: simpleCharacterArgument | character | groupChat | string | null,
      alwaysExistChar: character | groupChat | simpleCharacterArgument,
      chatID = -1,
      captureContext?: TranslateHTMLContext,
  ): string {
      if (charArg === '') return text

      const db = captureContext?.scriptContext.parserContext.database ?? getDatabase()
      let scripts: customscript[] = []
      scripts = [
          ...(captureContext?.scriptContext.presetRegex ?? db.presetRegex ?? []),
          ...(captureContext?.scriptContext.moduleRegexScripts ?? getModuleRegexScripts() ?? []),
          ...(alwaysExistChar?.customscript ?? []),
      ]

      const parsedScripts: pEdittransScript[] = []
      let orderChanged = false

      for (const script of scripts) {
          if (script.type !== 'edittrans') {
              continue
          }
          if (!script.in) {
              continue
          }

          let flag = 'g'
          if (script.ableFlag) {
              flag = script.flag || 'g'
          }

          let order = 0
          const actions: string[] = []

          //parse custom flags, same as processScriptFull
          flag = flag.replace(/<(.+?)>/g, (v: string, p1: string) => {
              const meta = p1.split(',').map((v) => v.trim())
              for (const m of meta) {
                  if (m.startsWith('order ')) {
                      order = parseInt(m.substring(6))
                      orderChanged = true
                  }
                  else {
                      actions.push(m)
                  }
              }

              return ''
          })

          if (actions.includes('move_top') || actions.includes('move_bottom')) {
              flag = flag.replace('g', '') //temperary fix
          }

          //remove unsupported flag
          flag = flag.trim().replace(/[^dgimsuvy]/g, '')

          //remove repeated flags
          flag = flag.split('').filter((v, i, a) => a.indexOf(v) === i).join('')

          if (flag.length === 0) {
              flag = 'u'
          }

          parsedScripts.push({ script, flag, order, actions })
      }

      if (orderChanged) {
          parsedScripts.sort((a, b) => b.order - a.order) //sort by order
      }

      for (const pscript of parsedScripts) {
          try {
              const script = pscript.script

              let input = script.in
              if (pscript.actions.includes('cbs')) {
                  const parser = captureContext?.scriptContext.parserContext
                  input = risuChatParser(input, parser ? {
                      chatID,
                      projectedChatID: captureContext.projectedChatID,
                      historyOffset: parser.historyOffset,
                      cbsConditions: captureContext.cbsConditions,
                      db: parser.database,
                      chara: captureContext.chara ?? parser.character,
                      userName: parser.userName,
                      personaPrompt: parser.personaPrompt,
                      modules: parser.modules,
                      moduleLorebooks: parser.moduleLorebooks,
                      selectedCharID: parser.selectedCharID,
                      chatVariables: parser.chatVariables,
                      globalChatVariables: parser.globalChatVariables,
                      currentTime: parser.currentTime,
                      triggerId: parser.triggerId,
                  } : { chatID })
              }

              const reg = new RegExp(input, pscript.flag)
              const outScript = script.out.replaceAll("$n", "\n")

              if (pscript.actions.includes('move_top') || pscript.actions.includes('move_bottom')) {
                  const isGlobal = pscript.flag.includes('g')
                  const matchAll = isGlobal ? text.matchAll(reg) : [text.match(reg)]
                  text = text.replace(reg, "")
                  for (const matched of matchAll) {
                      if (matched) {
                          const inData = matched[0]
                          const out = outScript
                              .replace(/(?<!\$)\$[0-9]+/g, (v) => {
                                  const index = parseInt(v.substring(1))
                                  if (index < matched.length) {
                                      return matched[index]
                                  }
                                  return v
                              })
                              .replace(/\$\&/g, inData)
                              //kept identical to processScriptFull, where parseInt on a group name never resolves
                              .replace(/(?<!\$)\$<([^>]+)>/g, (v) => {
                                  const groupName = parseInt(v.substring(2, v.length - 1))
                                  if (matched.groups && matched.groups[groupName]) {
                                      return matched.groups[groupName]
                                  }
                                  return v
                              })
                          if (pscript.actions.includes('move_top')) {
                              text = out + '\n' + text
                          }
                          else {
                              text = text + '\n' + out
                          }
                      }
                  }
              }
              else {
                  text = text.replace(reg, outScript)
              }
          } catch (error) {
              console.error(error)
          }
      }
      return text
  }
