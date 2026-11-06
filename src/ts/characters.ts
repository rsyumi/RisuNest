import { imageAliases, imageFormatFromBytes, stripImageExtension } from './media/imageFormats'
import { offerHtmlClipboardExport } from './htmlClipboardExport'
import { defaultChatToggleBinding } from './toggleBindings'
import { materializeAuthorNote } from './authorNotePresets'
import DOMPurify from 'dompurify'
import { get, writable } from "svelte/store";
import { saveImage, type character, type Chat, defaultSdDataFunc, type loreBook, getDatabase, getCharacterByIndex, setCharacterByIndex } from "./storage/database.svelte";
import { alertAddCharacter, alertCheckboxConfirm, alertError, alertNormal, alertSelect, alertStore, alertToast, alertWait } from "./alert";
import { language } from "../lang";
import { isArchivedCharacter } from "./storage/workingSetCatalog";
import { restoreArchivedCharacterWithConfirmation } from "./storage/characterArchive";
import { checkNullish, findCharacterbyId, getUserName, selectMultipleFile, selectSingleFile, mapSelectedFiles, fileAccept } from "./util";
import { v4 as uuidv4, v4 } from 'uuid';
import { DBState, MobileGUIStack, OpenRealmStore, selectedCharID } from "./stores.svelte";
import { AppendableBuffer, changeChatTo, checkCharOrder, createChatCopyName, downloadFile, getFileSrc } from "./globalApi.svelte";
import { updateInlayScreen } from "./process/inlayScreen";
import { parseMarkdownSafe } from "./parser/parser.svelte";
import { translateHTML } from "./translator/translator";
import { beginMobileBackgroundTask, type MobileBackgroundTask } from "./mobileBackgroundTask";
import { doingChat } from "./process/index.svelte";
import { importCharacter } from "./characterCards";
import { PngChunk } from "./pngChunk";
import {
    acquireCompleteConversation,
    activateCharacter,
    captureSelectedConversationTarget,
    commitCharacterAddition,
    deactivateActiveWorkingSet,
    deletePersistentCharacter,
    editWindowedChatList,
    fencePersistentNavigation,
    flushPendingData,
    getPersistentNavigationGeneration,
    getSelectedConversationMode,
    mutatePersistentCharacterDetail,
    readPersistentCompleteCharacter,
    readPersistentConversation,
    reconcilePersistentActiveCharacterIds,
} from "./storage/persistentDataRuntime.svelte";
import { removeCharacterIdFromOrder } from './storage/characterOrderMutation'
import { safeStructuredClone } from './polyfill'
import { isConversationSummaryStub } from './storage/conversationResidency'
import { isMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
import { open } from './storage/fileSelection'
import { readFile, open as openFile } from '@tauri-apps/plugin-fs'
import { isTauriDesktop } from './platform'
import { getPersistentDataStore } from './storage/persistentDataStoreFactory'
import type { CharacterDetail } from './storage/persistentDataStore'
import {
    archiveCurrentCharacterImage,
    selectCharacterImageFile,
} from './storage/characterImageFileRoute'
import { importNativeJpegAsset } from './storage/nativeJpegAssetImport'
import { runContentImport } from './storage/contentImportOperation'
import { SelectedConversationPromotionStaleError } from './storage/activeWorkingSet.svelte'
import { PersistentMutationFencedError } from './storage/saveCoordinator'
import { beginNavigationActivity } from './ui/navigationActivity'
import { yieldToUi } from './ui/yieldToUi'
import { estimateCharacterBytes } from './storage/conversationInsertPages'
import { forgetDisplayRecovery } from './displayRecovery.svelte'

export async function commitDetachedCharacter(
    character: character,
    reason: string,
): Promise<string> {
    const characterId = character.chaId
    await commitCharacterAddition({
        characterId,
        estimatedBytes: estimateCharacterBytes(character),
        install() {
            DBState.db.characters.push(character)
            checkCharOrder()
        },
    }, reason)
    return characterId
}

export async function createNewCharacter(): Promise<string> {
    return commitDetachedCharacter(createBlankChar(), 'create-character')
}



export async function getCharImage(loc:string, type:'plain'|'css'|'contain'|'lgcss') {
    const db = DBState.db
    
    // Return placeholder when hideAllImages is enabled
    if(db.hideAllImages){
        if(type === 'plain'){
            return '/none.webp'
        }
        return ''  // For CSS types, return empty to show default ? icon
    }
    
    if(!loc || loc === ''){
        if(type ==='css'){
            return ''
        }
        return null
    }
    const filesrc = await getFileSrc(loc)
    if(type === 'plain'){
        return filesrc
    }
    else if(type ==='css'){
        return `background: url("${filesrc}");background-size: cover;`
    }
    else if(type === 'lgcss'){
        return `background: url("${filesrc}");background-size: cover;height: 10.66rem;`

    }

    else{
        return `background: url("${filesrc}");background-size: contain;background-repeat: no-repeat;background-position: center;`
    }
}

export async function selectCharImg(charIndex:number, check?: (character: CharacterDetail) => void) {
    const characterId = DBState.db.characters[charIndex]?.chaId
    if (!characterId) return
    await selectCharacterImageFile(characterId, {
        isTauriDesktop,
        chooseDesktopPath: async () => {
            const selected = await open({
                multiple: false,
                directory: false,
                filters: fileAccept(imageAliases) === '*' ? undefined : [{
                    name: imageAliases.join(', '),
                    extensions: imageAliases,
                }],
            })
            return typeof selected === 'string' ? selected : null
        },
        readDesktopPath: readFile,
        onImageError: message => { void alertError(message) },
        readDesktopHeader: async path => {
            const file = await openFile(path, { read: true })
            try {
                const header = new Uint8Array(3)
                let offset = 0
                while (offset < header.length) {
                    const read = await file.read(header.subarray(offset))
                    if (!read) break
                    offset += read
                }
                return header.subarray(0, offset)
            } finally { await file.close() }
        },
        chooseLegacyFile: async () => selectSingleFile(imageAliases, { image: true }),
        nativeImport: input => runContentImport(input.displayName, {}, options => importNativeJpegAsset(input, {
            ...options,
            beforeStart: check ? async () => {
                const current = await getPersistentDataStore().readCharacter(characterId)
                if (!current) throw new Error('Character is unavailable')
                check(current.value)
            } : undefined,
        })),
        legacyImport: async (img) => {
            const type = imageFormatFromBytes(img)?.ext
            const pngExif: Record<string, string> = {}
            try {
                if(type === 'png'){
                    const gen = PngChunk.readGenerator(img)
                    const allowedChunk = [
                        'parameters', 'Comment', 'Title', 'Description', 'Author', 'Software', 'Source', 'Disclaimer', 'Warning', 'Copyright',
                    ]
                    for await (const chunk of gen){
                        if(chunk instanceof AppendableBuffer){
                            continue
                        }
                        if(!chunk){
                            continue
                        }
                        if(chunk.value.length > 20_000){
                            continue
                        }
                        if(allowedChunk.includes(chunk.key)){
                            pngExif[chunk.key] = chunk.value
                        }
                    }
                }
            } catch (error) {
                console.error(error)
            }
            const imgp = await saveImage(img)
            await mutatePersistentCharacterDetail(characterId, 'select-character-image', ({ character }) => {
                check?.(character)
                archiveCurrentCharacterImage(character)
                character.image = imgp
                if (Object.keys(pngExif).length > 0) {
                    character.extentions ??= {}
                    character.extentions.pngExif = { ...character.extentions.pngExif, ...pngExif }
                }
            })
            return imgp
        },
    })
}

export function dumpCharImage(charIndex:number) {
    const char = DBState.db.characters[charIndex] as character
    if(!char.image || char.image === ''){
        return
    }
    archiveCurrentCharacterImage(char)
    DBState.db.characters[charIndex] = char
}

export function changeCharImage(charIndex:number,changeIndex:number) {
    const char = DBState.db.characters[charIndex] as character
    const image = char.ccAssets[changeIndex].uri
    char.ccAssets.splice(changeIndex, 1)
    dumpCharImage(charIndex)
    char.image = image
    DBState.db.characters[charIndex] = char
}


export const addingEmotion = writable(false)

export async function addCharEmotion(charId:number) {
    const characterId = DBState.db.characters[charId]?.chaId
    if (!characterId || get(addingEmotion)) return
    addingEmotion.set(true)
    try {
        const failures: string[] = []
        const selected = await selectMultipleFile(imageAliases, { image: true, failures })
        if (!selected?.length) return
        const images: [string, string][] = []
        await mapSelectedFiles(selected, async f => {
            const imgp = await saveImage(f.data)
            images.push([stripImageExtension(f.name), imgp])
        }, failures)
        if (!images.length) return
        await mutatePersistentCharacterDetail(characterId, 'add-character-emotions', ({ character }) => {
            character.emotionImages.push(...images)
        })
    } finally {
        addingEmotion.set(false)
    }
}

export function rmCharEmotion(charId:number, emotionId:number) {
    let dbChar = DBState.db.characters[charId]
    dbChar.emotionImages.splice(emotionId, 1)
    DBState.db.characters[charId] = dbChar
}

function escapeHtmlText(value: unknown) {
    return String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}

// The same filter as parseMarkdownSafe.
const exportedChatSanitizeConfig = {
    FORBID_TAGS: ["a", "style"],
    FORBID_ATTR: ["style", "href", "class"]
}

/** Replaces a name case-insensitively in text and attribute values, never in the markup itself. */
function hideNameInExportedHtml(html: string, name: string) {
    if (!name) return html
    const pattern = new RegExp(name.replace(/[-\/\\^$*+\?\.()|[\]{}]/g, '\\$&'), 'gi')
    const template = document.createElement('template')
    template.innerHTML = html
    const walker = document.createTreeWalker(template.content, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT)
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        if (node.nodeType === Node.TEXT_NODE) {
            node.nodeValue = node.nodeValue.replace(pattern, '×××')
            continue
        }
        for (const attribute of (node as Element).attributes) {
            attribute.value = attribute.value.replace(pattern, '×××')
        }
    }
    return DOMPurify.sanitize(template.innerHTML, exportedChatSanitizeConfig)
}

export async function exportChat(page:number){
    // Translating every message can take long, so it holds the background task until the save step.
    let translation: MobileBackgroundTask | Promise<MobileBackgroundTask> | undefined
    const endTranslation = async () => {
        const task = await translation
        translation = undefined
        await task?.dispose()
    }
    try {

        const mode = await alertSelect(['Export as JSON', "Export as TXT", "Export as HTML File", "Export as HTML Embed"])
        const doTranslate = (mode === '2' || mode === '3') ? (await alertSelect([language.translateContent, language.doNotTranslate])) === '0' : false
        const anonymous = (mode === '2' || mode === '3') ? ((await alertSelect([language.includePersonaName, language.hidePersonaName])) === '1') : false
        const selectedID = get(selectedCharID)
        const db = DBState.db
        const char = db.characters[selectedID]
        const chatId = char.chats[page]?.id
        if (!chatId) throw new Error('Chat was not found')
        const chat = await readPersistentConversation(char.chaId, chatId, 'export-chat')
        if (!chat) throw new Error('Chat was not found')
        const date = new Date().toJSON();
        const htmlChatParse = async (v:string) => {
            v = parseMarkdownSafe(v)

            if(doTranslate){
                translation ??= beginMobileBackgroundTask('export', undefined, true)
                // Translation can return new markup, so it gets the same filter as parseMarkdownSafe.
                v = DOMPurify.sanitize(await translateHTML(v, false, '', -1), exportedChatSanitizeConfig)
            }

            if(anonymous){
                v = hideNameInExportedHtml(v, char.name)
            }

            return v
        }

        if(mode === '0'){
            let folders = []
            if(chat.folderId) {
                folders = db.characters[selectedID].chatFolders?.filter(f => f.id === chat.folderId)
            }
            const stringl = Buffer.from(JSON.stringify({
                type: 'risuChat',
                ver: 2,
                data: materializeAuthorNote(db.authorNotePresets, chat),
                folders: folders
            }), 'utf-8')
    
            await downloadFile(`${char.name}_${date}_chat`.replace(/[<>:"/\\|?*\.\,]/g, "") + '.json', stringl)
    
        }
        else if(mode === '2'){

            let chatContentHTML = ''

            let i = 0
            for(const v of chat.message){
                alertWait(`Translating... ${i++}/${chat.message.length}`)
                const name = v.saying ? findCharacterbyId(v.saying).name : v.role === 'char' ? char.name : anonymous ? '×××' : getUserName()
                chatContentHTML += `<div class="chat">
                    <h2>${escapeHtmlText(name)}</h2>
                    <div>${await htmlChatParse(v.data)}</div>
                </div>`
            }

            const doc = `
                <!DOCTYPE html>
                <html>
                    <head>
                        <title>${escapeHtmlText(char.name)} Chat</title>
                        <style>
                            body{
                                font-family: Arial, sans-serif;
                                display: flex;
                                justify-content: center;
                            }
                            .container{
                                max-width: 800px;
                                padding: 1rem;
                                border-radius: 10px;
                                display: flex;
                                flex-direction: column;
                                gap: 1rem;
                            }
                            .chat{
                                background: #f0f0f0;
                                padding: 1rem;
                                border-radius: 10px;
                                display: flex;
                                flex-direction: column;
                            }
                            .idat{
                                display: none;
                            }
                            h2{
                                margin: 0;
                            }
                            .chat div{
                                margin-top: 0.5rem;
                                break-word: break-all;
                            }
                        </style>
                    </head>
                    <body>
                        <div class="container">
                            <div class="chat">
                                <h2>${escapeHtmlText(char.name)}</h2>
                                <div>${await htmlChatParse(
                                    chat.fmIndex === -1 ? char.firstMessage : char.alternateGreetings?.[chat.fmIndex ?? 0]
                                )}</div>
                            </div>
                            ${chatContentHTML}
                        </div>
                        <div class="idat">${
                            JSON.stringify(chat).replace(/</g, '&lt;').replace(/>/g, '&gt;')
                        }</div>
                    </body>
            `


            await endTranslation()
            await downloadFile(`${char.name}_${date}_chat`.replace(/[<>:"/\\|?*\.\,]/g, "") + '.html', Buffer.from(doc, 'utf-8'))
        }
        else if(mode === '3'){
            //create a html table
            let chatContentHTML = ''

            let i = 0
            for(const v of chat.message){
                alertWait(`Translating... ${i++}/${chat.message.length}`)
                const name = v.saying ? findCharacterbyId(v.saying).name : v.role === 'char' ? char.name : anonymous ? '×××' : getUserName()
                chatContentHTML += `<tr>
                    <td>${escapeHtmlText(name)}</td>
                    <td>${await htmlChatParse(v.data)}</td>
                </tr>`
            }

            const template = `
                <table>
                    <tr>
                        <th>Character</th>
                        <th>Message</th>
                    </tr>
                    <tr>
                        <td>${escapeHtmlText(char.name)}</td>
                        <td>${await htmlChatParse(char.firstMessage)}</td>
                    </tr>
                    ${chatContentHTML}
                </table>
                <p>Chat from RisuNest</p>
            `

            await endTranslation()
            offerHtmlClipboardExport(template, `${char.name}_${date}_chat`.replace(/[<>:"/\\|?*\.\,]/g, "") + '.html', {
                present: value => alertStore.set(value),
                download: downloadFile,
                success: copied => alertNormal(copied ? language.clipboardSuccess : language.successExport),
                error: error => alertError(String(error)),
                labels: { copy: language.copy, download: language.download, cancel: language.cancel },
            })
            return

        }
        else{
            
            let stringl = chat.message.map((v) => {
                if(v.saying){
                    return `--${findCharacterbyId(v.saying).name}\n${v.data}`
                }
                else{
                    return `--${v.role === 'char' ? char.name : getUserName()}\n${v.data}`
                }
            }).join('\n\n')

            stringl = `--${char.name}\n${char.firstMessage}\n\n` + stringl

            await downloadFile(`${char.name}_${date}_chat`.replace(/[<>:"/\\|?*\.\,]/g, "") + '.txt', Buffer.from(stringl, 'utf-8'))

        }
        alertNormal(language.successExport)
    } catch (error) {
        alertError(error)
    } finally {
        await endTranslation()
    }
}

export async function importChat(){
    const dat =await selectSingleFile(['json','jsonl','txt','html'])
    if(!dat){
        return
    }
    try {
        const selectedID = get(selectedCharID)
        const characterId = DBState.db.characters[selectedID].chaId

        if(dat.name.endsWith('jsonl')){
            const lines = Buffer.from(dat.data).toString('utf-8').split('\n')
            let newChat:Chat = {
                message: [],
                note: "",
                name: "Imported Chat",
                localLore: [],
                fmIndex: -1,
                id: v4()
            }

            let isFirst = true
            for(const line of lines){
                
                const presedLine = JSON.parse(line)
                if(presedLine.name && presedLine.is_user, presedLine.mes){
                    if(!isFirst){
                        newChat.message.push({
                            role: presedLine.is_user ? "user" : 'char',
                            data: formatTavernChat(presedLine.mes, DBState.db.characters[selectedID].name),
                            chatId: v4(),
                        })
                    }
                }

                isFirst = false
            }

            if(newChat.message.length === 0){
                alertError(language.errors.noData)
                return
            }

            const imported = await editSelectedChatList(characterId, 'import-chat', (character) => {
                if(character.chatFolders
                    .filter(folder => folder.id === newChat.folderId).length === 0) {
                    newChat.folderId = null
                }
                character.chats.unshift(newChat)
                return newChat.id
            })
            if(imported){
                alertNormal(language.successImport)
            }
        }
        else if(dat.name.endsWith('json')){
            // The Buffer polyfill collects every code unit in an array before decoding,
            // which needs several times the file size; a large chat file runs the renderer out of memory.
            const json = JSON.parse(new TextDecoder('utf-8', { ignoreBOM: true }).decode(dat.data))
            if((json.type === 'risuAllChats' || json.type === 'risuChat') && json.ver === 2){
                const folders = json.folders || []
                const chats = Array.isArray(json.data) ? json.data : [json.data]
                const imported = await editSelectedChatList(characterId, 'import-chat', (character) => {
                    let folderIdMap = {}
                    folders.forEach(folder => {
                        if(character.chatFolders?.some(f => f.id === folder.id)){
                            const newId = uuidv4()
                            folderIdMap[folder.id] = newId
                            folder.id = newId
                        } else {
                            folderIdMap[folder.id] = folder.id
                        }
                    })
                    if(character.chatFolders === undefined){
                        character.chatFolders = []
                    }
                    character.chatFolders.push(...folders)
                    chats.forEach(chat => {
                        if(chat.folderId && folderIdMap[chat.folderId]){
                            chat.folderId = folderIdMap[chat.folderId]
                        }
                        chat.id = v4()
                    })
                    character.chats.unshift(...chats)
                    return null
                })
                if(imported){
                    alertNormal(language.successImport)
                }
                return
            }
            if(json.type === 'risuAllChats' && json.ver === 1){
                const chats = json.data
                if(Array.isArray(chats) && chats.length > 0){
                    const imported = await editSelectedChatList(characterId, 'import-chat', (character) => {
                        const usedIds = new Set(character.chats.map((chat) => chat.id))
                        character.chats.unshift(...(chats.map((v) => {
                            if(!v.id || usedIds.has(v.id)){
                                v.id = uuidv4()
                            }
                            usedIds.add(v.id)
                            if(!v.localLore){
                                v.localLore = []
                            }
                            v.fmIndex ??= -1
                            return v
                        })))
                        return null
                    })
                    if(imported){
                        alertNormal(language.successImport)
                    }
                    return
                } else {
                    alertError(language.errors.noData)
                    return
                }
            }
            if(json.type === 'risuChat' && json.ver === 1){
                const das:Chat = json.data
                if(!(checkNullish(das.message) || checkNullish(das.note) || checkNullish(das.name) || checkNullish(das.localLore))){
                    das.fmIndex ??= -1
                    das.id = v4()
                    const imported = await editSelectedChatList(characterId, 'import-chat', (character) => {
                        character.chats.unshift(das)
                        return null
                    })
                    if(imported){
                        alertNormal(language.successImport)
                    }
                    return
                }
                else{
                    alertError(language.errors.noData)
                    return   
                }
            }
            else{
                alertError(language.errors.noData)
                return
            }
        }
        else if(dat.name.endsWith('html')){
            const doc = new DOMParser().parseFromString(Buffer.from(dat.data).toString('utf-8'), 'text/html')
            const chat = doc.querySelector('.idat').textContent
            const json = JSON.parse(chat)
            if(json.message && json.note && json.name && json.localLore){
                json.id = v4()
                const imported = await editSelectedChatList(characterId, 'import-chat', (character) => {
                    character.chats.unshift(json)
                    return null
                })
                if(imported){
                    alertNormal(language.successImport)
                }
            }
            else{
                alertError(language.errors.noData)
            }
        }
    } catch (error) {
        alertError(error)
    }
}

export async function exportAllChats() {
    try {
        const selectedID = get(selectedCharID)
        const db = getDatabase()
        const selected = db.characters[selectedID]
        const char = selected
            ? await readPersistentCompleteCharacter(selected.chaId, 'export-all-chats')
            : null
        if (!char) throw new Error('Character was not found')
        const date = new Date().toISOString().replace(/[:.]/g, "-")
        const allChats = char.chats.map((chat) => materializeAuthorNote(db.authorNotePresets, chat))
        const allFolders = char.chatFolders
        const stringl = Buffer.from(JSON.stringify({
            type: 'risuAllChats',
            ver: 2,
            data: allChats,
            folders: allFolders
        }), 'utf-8')
        await downloadFile(`${char.name}_all_chats_${date}`.replace(/[<>:"/\\|?*.,]/g, "") + '.json', stringl)
        alertNormal(language.successExport)
    } catch (error) {
        alertError(error)
    }
}

function formatTavernChat(chat:string, charName:string){
    const db = getDatabase()
    return chat.replace(/<([Uu]ser)>|\{\{([Uu]ser)\}\}/g, getUserName()).replace(/((\{\{)|<)([Cc]har)(=.+)?((\}\})|>)/g, charName)
}

export function characterFormatUpdate(indexOrCharacter:number|character, arg:{
    updateInteraction?:boolean,
} = {}){
    let cha = typeof(indexOrCharacter) === 'number' ? getCharacterByIndex(indexOrCharacter) : indexOrCharacter
    if(cha.chats.length === 0){
        cha.chats = [{
            message: [],
            note: '',
            name: 'Chat 1',
            localLore: [],
            id: v4()
        }]
    }
    if(!cha.chats[cha.chatPage]){
        cha.chatPage = 0
    }
    if(
        !isMetadataOnlySelectedConversation(cha.chats[cha.chatPage]) &&
        !cha.chats[cha.chatPage].message
    ){
        cha.chats[cha.chatPage].message = []
    }
    if(!cha.type){
        cha.type = 'character'
    }
    if(!cha.chaId){
        cha.chaId = uuidv4()
    }
    if(checkNullish(cha.sdData)){
        cha.sdData = defaultSdDataFunc()
    }
    if(checkNullish(cha.utilityBot)){
        cha.utilityBot = false
    }
    cha.triggerscript = cha.triggerscript ?? []
    cha.alternateGreetings = cha.alternateGreetings ?? []
    cha.exampleMessage = cha.exampleMessage ?? ''
    cha.creatorNotes = cha.creatorNotes ?? ''
    cha.systemPrompt = cha.systemPrompt ?? ''
    cha.tags = cha.tags ?? []
    cha.creator = cha.creator ?? ''
    cha.characterVersion = cha.characterVersion ?? ''
    cha.personality = cha.personality ?? ''
    cha.scenario = cha.scenario ?? ''
    cha.firstMsgIndex = cha.firstMsgIndex ?? -1
    cha.additionalData = cha.additionalData ?? {
        tag: [],
        creator: '',
        character_version: ''
    }
    cha.voicevoxConfig = cha.voicevoxConfig ?? {
        SPEED_SCALE: 1,
        PITCH_SCALE: 0,
        INTONATION_SCALE: 1,
        VOLUME_SCALE: 1
    }
    if(cha.postHistoryInstructions){
        cha.chats[cha.chatPage].note += "\n" + cha.postHistoryInstructions
        cha.chats[cha.chatPage].note = cha.chats[cha.chatPage].note.trim()
        cha.postHistoryInstructions = null
    }
    cha.additionalText ??= ''
    cha.depth_prompt ??= {
        depth: 0,
        prompt: ''
    }
    cha.hfTTS ??= {
        model: '',
        language: 'en'
    }
    cha.backgroundHTML ??= ''
    cha.backgroundCSS ??= ''
    cha.creation_date ??= Date.now()
    cha.globalLore = updateLorebooks(cha.globalLore)
    if(!cha.newGenData){
        cha = updateInlayScreen(cha)
    }
    // Migrate legacy 'none' value to '' for UI dropdown compatibility
    // Using '' because it's falsy, so `if (ttsMode)` correctly detects enabled TTS
    if (cha.ttsMode === 'none') {
        cha.ttsMode = ''
    }
    cha.ttsMode ??= ''
    if(checkNullish(cha.customscript)){
        cha.customscript = []
    }
    cha.lastInteraction = Date.now()
    if(typeof(indexOrCharacter) === 'number'){
        setCharacterByIndex(indexOrCharacter, cha)
    }
    for(let i = 0; i < cha.chats.length; i++){
        const chat = cha.chats[i]
        if(isConversationSummaryStub(chat)) continue
        chat.fmIndex ??= cha.firstMsgIndex ?? -1
        if(!chat.id){
            chat.id = uuidv4()
        }
        if(!chat.localLore){
            chat.localLore = []
        }
    }
    return cha
}

export function updateLorebooks(book:loreBook[]){
    return book.map((v) => {
        // Imports, plugins and scripts can store an entry without these, which
        // the editor and the prompt builder read as strings.
        v.key ??= ''
        v.comment ??= ''
        v.content ??= ''
        v.bookVersion ??= 1
        if(v.bookVersion >= 2){
            return v
        }
        if(v.activationPercent){
            const perc = v.activationPercent
            v.activationPercent = null

            v.content = `@@probability ${perc}\n${v.content}`
        }
        v.content = v.content.replace(/@@@?end/g, '@@depth 0').replace(/\<(char|bot)\>/g, '{{char}}').replace(/\<(user)\>/g, '{{user}}')
        v.bookVersion = 2
        return v
    })

}

export function createBlankChar():character{
    return {
        name: '',
        firstMessage: '',
        desc: '',
        notes: '',
        chats: [{
            ...defaultChatToggleBinding(DBState.db),
            message: [],
            note: '',
            name: 'Chat 1',
            localLore: [],
            id: v4()
        }],
        chatFolders: [],
        chatPage: 0,
        emotionImages: [],
        bias: [],
        viewScreen: 'none',
        globalLore: [],
        chaId: uuidv4(),
        type: 'character',
        sdData: defaultSdDataFunc(),
        utilityBot: false,
        customscript: [],
        exampleMessage: '',
        creatorNotes:'',
        systemPrompt:'',
        postHistoryInstructions:'',
        alternateGreetings:[],
        tags:[],
        creator:"",
        characterVersion: '',
        personality:"",
        scenario:"",
        firstMsgIndex: -1,
        replaceGlobalNote: "",
        triggerscript: [{
            comment: "",
            type: "manual",
            conditions: [],
            effect: [{
                type: "v2Header",
                code: "",
                indent: 0
            }]
        }, {
            comment: "New Event",
            type: 'manual',
            conditions: [],
            effect: []
        }],
        additionalText: ''
    }
}




function dataURLtoBuffer(string:string){
    const regex = /^data:.+\/(.+);base64,(.*)$/;

    const matches = string.match(regex);
    const ext = matches[1];
    const data = matches[2];
    return Buffer.from(data, 'base64');
}

export async function removeChar(identifier:string|number,name:string, type:'normal'|'permanent'|'permanentForce' = 'normal'){
    const liveDatabase = getDatabase()
    const targetId = typeof identifier === 'string'
        ? identifier
        : liveDatabase.characters[identifier]?.chaId
    if (!targetId) return
    if(type !== 'permanentForce'){
        if (!(await alertCheckboxConfirm({
            title: language.removeConfirm + name,
            description: type === 'normal'
                ? language.checkboxConfirmation.characterTrashDescription
                : language.checkboxConfirmation.characterDeletionDescription,
            checkboxLabel: language.checkboxConfirmation.characterDeletion,
            actionLabel: language.confirm,
            cancelLabel: language.cancel,
            requireChecked: true,
        })).confirmed) return
    }
    const selected = liveDatabase.characters[get(selectedCharID)]
    const targetTrashTime = liveDatabase.characters.find(
        (character) => character.chaId === targetId,
    )?.trashTime
    const selectedCharacterId = selected?.chaId ?? null
    if (!await deactivateActiveWorkingSet()) return
    const deactivatedGeneration = getPersistentNavigationGeneration()
    // Releasing the selected character clears the selection, so an empty
    // selection still belongs to this removal unless navigation moved on.
    const restoreSelectionAfterFailedMutation = async () => {
        const currentSelectedId = DBState.db.characters[get(selectedCharID)]?.chaId ?? null
        if (
            !selectedCharacterId ||
            (currentSelectedId !== selectedCharacterId && currentSelectedId !== null) ||
            getPersistentNavigationGeneration() !== deactivatedGeneration
        ) return
        const beforeActivationGeneration = getPersistentNavigationGeneration()
        let restored = false
        try {
            restored = await activateCharacter(selectedCharacterId)
        } catch {
            restored = false
        }
        if (restored) return
        const afterActivationGeneration = getPersistentNavigationGeneration()
        const selectedAfterFailure = DBState.db.characters[get(selectedCharID)]?.chaId ?? null
        if (
            (selectedAfterFailure === selectedCharacterId || selectedAfterFailure === null) &&
            (
                afterActivationGeneration === beforeActivationGeneration ||
                afterActivationGeneration === beforeActivationGeneration + 1
            )
        ) {
            selectedCharID.set(-1)
            reconcilePersistentActiveCharacterIds(DBState.db, null)
        }
    }
    const clearSelectionAfterCommittedReplacement = async () => {
        const currentSelectedId = DBState.db.characters[get(selectedCharID)]?.chaId ?? null
        if (
            getPersistentNavigationGeneration() !== deactivatedGeneration ||
            (currentSelectedId !== selectedCharacterId && currentSelectedId !== null)
        ) return
        if (!await deactivateActiveWorkingSet()) return
        selectedCharID.set(-1)
        reconcilePersistentActiveCharacterIds(DBState.db, null)
    }
    const clearSelectionAfterCommittedDetail = () => {
        const currentSelectedId = DBState.db.characters[get(selectedCharID)]?.chaId ?? null
        if (
            getPersistentNavigationGeneration() !== deactivatedGeneration ||
            (currentSelectedId !== selectedCharacterId && currentSelectedId !== null)
        ) return
        selectedCharID.set(-1)
        reconcilePersistentActiveCharacterIds(DBState.db, null)
    }
    let changed = false
    try {
        if (type === 'normal') {
            changed = await mutatePersistentCharacterDetail(
                targetId,
                'character-removal',
                ({ root, character }) => {
                    removeCharacterIdFromOrder(root, targetId)
                    character.trashTime = Date.now()
                },
            )
        } else {
            changed = await deletePersistentCharacter(
                targetId,
                'character-removal',
            )
        }
    } catch (error) {
        const locallyCommitted = type !== 'normal' && !DBState.db.characters.some(
            (character) => character.chaId === targetId,
        )
        const locallyTrashed = type === 'normal' && DBState.db.characters.some(
            (character) => (
                character.chaId === targetId &&
                character.trashTime !== undefined &&
                character.trashTime !== targetTrashTime
            ),
        )
        if (locallyCommitted) {
            await clearSelectionAfterCommittedReplacement().catch(() => false)
        } else if (locallyTrashed) {
            clearSelectionAfterCommittedDetail()
        } else {
            await restoreSelectionAfterFailedMutation().catch(() => false)
        }
        throw error
    }
    if (!changed) {
        await restoreSelectionAfterFailedMutation()
        return
    }
    if (type !== 'normal') {
        void forgetDisplayRecovery((target) => target.characterId === targetId)
        await clearSelectionAfterCommittedReplacement()
    } else {
        clearSelectionAfterCommittedDetail()
    }
}

export async function addCharacter(arg:{
    reseter?:()=>any,
} = {}){
    const reseter = arg.reseter ?? (() => {})
    MobileGUIStack.set(100)
    let finalStack = 1
    try {
        const r = await alertAddCharacter()
        if(r === 'importFromRealm'){
            if (!await deactivateActiveWorkingSet()) return
            selectedCharID.set(-1)
            OpenRealmStore.set(true)
            finalStack = 0
            return
        }
        reseter();
        let addedCharacterId: string | null = null
        switch(r){
            case 'createfromScratch':
                addedCharacterId = await createNewCharacter()
                break
            case 'importCharacter':
                addedCharacterId = await importCharacter()
                break
            default:
                return
        }
        if(addedCharacterId){
            const currentIndex = getDatabase().characters.findIndex(
                (character) => character.chaId === addedCharacterId,
            )
            await changeChar(currentIndex)
        }
    } catch (error) {
        alertError(error)
    } finally {
        MobileGUIStack.set(finalStack)
    }
}

// Applied to the activation candidate rather than after selection, so the
// switch records the toggle with its other activation changes.
function applyHypaV3AlwaysToggleOn(candidate: character): character {
    const db = getDatabase()
    if (db.hypaV3 && db.hypaV3Presets?.[db.hypaV3PresetId]?.settings?.alwaysToggleOn) {
        candidate.supaMemory = true
    }
    return candidate
}

export async function changeChar(index: number, arg:{
    reseter?:()=>any,
} = {}): Promise<boolean> {
    const reseter = arg.reseter ?? (() => {})
    const target = DBState.db.characters?.[index]
    const chaId = target?.chaId
    // Reactivating the open character would reload it and reset the chat view.
    if(
        chaId &&
        get(selectedCharID) === index &&
        captureSelectedConversationTarget()?.characterId === chaId
    ){
        reseter()
        return true
    }
    if(get(doingChat)){
      alertToast(language.navigationBlockedWhileGenerating)
      return false
    }
    if(!chaId) return false
    if(isArchivedCharacter(target)){
        await restoreArchivedCharacterWithConfirmation(chaId)
        return false
    }
    const restoreNavigationGeneration = fencePersistentNavigation()
    const activity = beginNavigationActivity('character')
    const isRestoreCurrent = () =>
        activity.isCurrent() &&
        getPersistentNavigationGeneration() === restoreNavigationGeneration
    try {
        reseter()
        await yieldToUi()
        if (!isRestoreCurrent()) return false
        if (!isRestoreCurrent()) return false
        let expectedNavigationGeneration = restoreNavigationGeneration + 1
        let exhaustedGeneration: number | undefined
        const activationOptions = {
            onRetryExhausted: (generation: number) => { exhaustedGeneration = generation },
            normalize: (candidate: character) =>
                applyHypaV3AlwaysToggleOn(characterFormatUpdate(candidate, {
                    updateInteraction: true,
                })),
        }
        let activated = await activateCharacter(chaId, activationOptions)
        if (!activated && exhaustedGeneration === undefined) {
            if (!activity.isCurrent() || getPersistentNavigationGeneration() !== expectedNavigationGeneration) return false
            expectedNavigationGeneration += 1
            activated = await activateCharacter(chaId, activationOptions)
        }
        if (!activated) {
            if (activity.isCurrent() && getPersistentNavigationGeneration() === (exhaustedGeneration ?? expectedNavigationGeneration)) {
                alertToast(language.characterSelectionFailed)
            }
            return false
        }
        if (!activity.isCurrent()) return false
        return true
    } catch (error) {
        if (error instanceof PersistentMutationFencedError) {
            alertToast(language.navigationBlockedWhileEditingPaused)
        } else {
            alertError(error)
        }
        return false
    } finally {
        activity.finish()
    }
}

/**
 * Edits the selected character's chat list. A windowed selection records the
 * edit as evidence against the saved list; otherwise the selected conversation
 * is held complete, so neither path persists an unexplained list change. The
 * previously selected chat stays selected unless `edit` returns the id of the
 * chat to switch to. `edit` returns false to abandon the change. It may run
 * twice, first on a draft and again on the complete path when the draft edit
 * cannot be recorded.
 */
export async function editSelectedChatList(
    characterId: string,
    reason: string,
    edit: (character: character) => string | null | false,
): Promise<boolean> {
    const blockedByGeneration = () => {
        if (!get(doingChat)) return false
        alertToast(language.navigationBlockedWhileGenerating)
        return true
    }
    const resolveCharacter = () => {
        const current = DBState.db.characters[get(selectedCharID)]
        return current?.chaId === characterId ? current : null
    }
    if (!resolveCharacter() || blockedByGeneration()) return false
    const windowed = await editWindowedSelectedChatList(characterId, reason, resolveCharacter, edit)
    if (windowed !== 'unsupported') return windowed
    if (blockedByGeneration()) return false
    const target = captureSelectedConversationTarget()
    if (target && target.characterId !== characterId) return false
    let lease: Awaited<ReturnType<typeof acquireCompleteConversation>> | null = null
    if (target) {
        try {
            lease = await acquireCompleteConversation(reason, target)
        } catch (error) {
            if (error instanceof SelectedConversationPromotionStaleError) return false
            throw error
        }
    }
    try {
        if (blockedByGeneration()) return false
        const character = resolveCharacter()
        if (!character) return false
        const selectedId = character.chats[character.chatPage]?.id
        if (lease && selectedId !== target?.conversationId) return false
        const nextId = edit(character)
        if (nextId === false) return false
        const selectedIndex = character.chats.findIndex((chat) => chat.id === selectedId)
        if (selectedIndex !== -1 && character.chatPage !== selectedIndex) {
            character.chatPage = selectedIndex
        }
        if (lease) {
            try {
                await flushPendingData(reason)
            } catch (error) {
                // The edit stays in the working set and is saved once storage accepts input.
                if (error instanceof PersistentMutationFencedError) return false
                throw error
            }
        }
        return nextId === null ? true : await changeChatTo(nextId)
    } finally {
        lease?.release()
    }
}

async function editWindowedSelectedChatList(
    characterId: string,
    reason: string,
    resolveCharacter: () => character | null,
    edit: (character: character) => string | null | false,
): Promise<boolean | 'unsupported'> {
    const initial = captureSelectedConversationTarget()
    if (
        !initial ||
        initial.characterId !== characterId ||
        getSelectedConversationMode() !== 'windowed'
    ) return 'unsupported'
    const flush = async () => {
        try {
            await flushPendingData(reason)
            return true
        } catch (error) {
            // The edit stays in the working set and is saved once storage accepts input.
            if (error instanceof PersistentMutationFencedError) return false
            throw error
        }
    }
    // The edit is described against the saved list, so earlier changes go first.
    if (!(await flush())) return false
    const target = captureSelectedConversationTarget()
    if (
        !target ||
        target.characterId !== characterId ||
        target.conversationId !== initial.conversationId ||
        !resolveCharacter()
    ) return false
    const result = editWindowedChatList(target, edit)
    if (result.kind === 'unsupported') return 'unsupported'
    if (result.kind === 'refused') return false
    if (!(await flush())) return false
    return result.nextId === null ? true : await changeChatTo(result.nextId)
}

export async function addNewChat(character: character): Promise<boolean> {
    return editSelectedChatList(character.chaId, 'add-chat', (current) => {
        const chats = current.chats
        const newChat: Chat = {
            ...defaultChatToggleBinding(DBState.db),
            message: [],
            note: '',
            name: `New Chat ${chats.length + 1}`,
            localLore: [],
            fmIndex: -1,
            id: uuidv4(),
        }

        chats.unshift(newChat)
        current.chats = chats
        return newChat.id
    })
}

export async function duplicateChat(characterId: string, chatId: string): Promise<boolean> {
    const selectedBeforeRead = getDatabase().characters[get(selectedCharID)]?.chaId
    if (selectedBeforeRead !== characterId) return false
    const source = await readPersistentConversation(characterId, chatId, 'duplicate-chat')
    if (!source) return false
    return editSelectedChatList(characterId, 'duplicate-chat', (character) => {
        if (!character.chats.some((conversation) => conversation.id === chatId)) return false
        // The persistent reader returns an owned copy, including its message pages.
        const duplicate = { ...source }
        duplicate.name = createChatCopyName(duplicate.name, 'Copy')
        duplicate.id = v4()
        character.chats.unshift(duplicate)
        character.chats = character.chats
        return duplicate.id
    })
}

export async function removeChat(character: character, chatId: string): Promise<boolean> {
    const chats = character.chats
    if(!chats.some((chat) => chat.id === chatId)) return false
    const selectedChatId = chats[character.chatPage]?.id
    if (selectedChatId === chatId) {
        // Leave the chat first so the removal never targets the selected conversation.
        const survivingId = chats.find((candidate) => candidate.id !== chatId)?.id
        if (!survivingId) return false
        if (!(await changeChatTo(survivingId)) && !(await changeChatTo(survivingId))) return false
    }
    const removed = await editSelectedChatList(character.chaId, 'remove-chat', (current) => {
        const removeIndex = current.chats.findIndex((chat) => chat.id === chatId)
        if (removeIndex === -1 || removeIndex === current.chatPage) return false
        current.chats.splice(removeIndex, 1)
        current.chats = current.chats
        return null
    })
    if (removed) {
        void forgetDisplayRecovery((target) => target.characterId === character.chaId && target.conversationId === chatId)
    }
    return removed
}
