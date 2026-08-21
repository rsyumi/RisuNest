import { v4 as uuidv4 } from 'uuid'
import { defaultJailbreak, defaultMainPrompt, oldJailbreak, oldMainPrompt } from './defaultPrompts'
import {
    defaultSdDataFunc,
    normalizeDatabaseDefaults,
    type Database,
} from './database.svelte'
import { canonicalJson } from './saveCoordinator'

export interface DatabasePreparationOptions {
    createId?: () => string
    now?: number
}

export async function checkNewFormat(
    database: Database,
    options: { now?: number } = {},
): Promise<Database> {
    database.characters = database.characters
        .map((character) => {
            if (!character) return null
            character.type ??= 'character'
            character.chatPage ??= 0
            character.chats ??= []
            character.customscript ??= []
            character.firstMessage ??= ''
            character.globalLore ??= []
            character.name ??= ''
            character.viewScreen ??= 'none'
            character.emotionImages ??= []
            if (character.type === 'character') {
                character.bias ??= []
                character.characterVersion ??= ''
                character.creator ??= ''
                character.desc ??= ''
                character.utilityBot ??= false
                character.tags ??= []
                character.systemPrompt ??= ''
                character.scenario ??= ''
            }
            return character
        })
        .filter((character) => character !== null)

    database.modules = await Promise.all(
        (database.modules ?? []).map(async (moduleValue) => {
            if (!moduleValue?.lorebook) return moduleValue
            if (Array.isArray(moduleValue.lorebook)) {
                const { updateLorebooks } = await import('../characters')
                moduleValue.lorebook = updateLorebooks(moduleValue.lorebook)
                return moduleValue
            }

            const [{ language }, { alertConfirm, alertError, alertMd, waitAlert }] =
                await Promise.all([import('../../lang'), import('../alert')])
            console.error('Critical: Invalid lorebook format detected in module')
            console.error('Module data:', JSON.stringify(moduleValue, null, 2))
            alertError(
                language.bootstrap.dataCorruptionDetected(
                    moduleValue.name || 'Unknown',
                    typeof moduleValue.lorebook,
                ),
            )
            await waitAlert()
            if (await alertConfirm(language.bootstrap.reportErrorQuestion)) {
                try {
                    const diagnosticInfo = {
                        timestamp: new Date().toISOString(),
                        moduleName: moduleValue.name || 'Unknown',
                        lorebookType: typeof moduleValue.lorebook,
                        lorebookValue: JSON.stringify(moduleValue.lorebook).substring(0, 500),
                        isArray: Array.isArray(moduleValue.lorebook),
                        keys: moduleValue.lorebook
                            ? Object.keys(moduleValue.lorebook).join(', ')
                            : 'N/A',
                        formatVersion: database.formatversion || 'Unknown',
                    }
                    const reportData = JSON.stringify(diagnosticInfo, null, 2)
                    await alertMd(language.bootstrap.diagnosticInformation(reportData))
                    await waitAlert()
                    console.log('Diagnostic information for developers:', diagnosticInfo)
                } catch (error) {
                    console.error('Failed to generate diagnostic report:', error)
                }
            }
            if (await alertConfirm(language.bootstrap.resetLorebookQuestion)) {
                moduleValue.lorebook = []
                console.log('Lorebook reset to empty array by user choice')
            } else {
                console.warn('User chose to keep corrupted lorebook data')
            }
            return moduleValue
        }),
    )
    database.modules = database.modules.filter(
        (moduleValue) => moduleValue !== null && moduleValue !== undefined,
    )

    database.personas = (database.personas ?? [])
        .map((persona) => {
            persona.id ??= uuidv4()
            return persona
        })
        .filter((persona) => persona !== null && persona !== undefined)

    if (!database.formatversion) {
        const cleanAssetPath = (value: string) => {
            if (value.startsWith('assets') || value.length < 3) return value
            return 'assets/' + value.replace(/\\/g, '/').split('assets/')[1] || value
        }
        database.customBackground = cleanAssetPath(database.customBackground)
        database.userIcon = cleanAssetPath(database.userIcon)
        for (const character of database.characters) {
            if (character.image) character.image = cleanAssetPath(character.image)
            for (const emotionImage of character.emotionImages ?? []) {
                if (emotionImage?.length >= 2) emotionImage[1] = cleanAssetPath(emotionImage[1])
            }
        }
        database.formatversion = 2
    }
    if (database.formatversion < 3) {
        for (const character of database.characters) {
            if (character.type === 'character' && character.sdData == null) {
                character.sdData = defaultSdDataFunc()
            }
        }
        database.formatversion = 3
    }
    if (database.formatversion < 4) database.formatversion = 4
    if (database.formatversion < 5) {
        if (database.loreBookToken < 8000) database.loreBookToken = 8000
        database.formatversion = 5
    }
    database.characterOrder ??= []
    if (database.mainPrompt === oldMainPrompt) database.mainPrompt = defaultMainPrompt
    if (database.mainPrompt === oldJailbreak) database.mainPrompt = defaultJailbreak

    const now = options.now ?? Date.now()
    database.characters = database.characters.filter((character) => {
        if (!character.trashTime) return true
        return character.trashTime + 1000 * 60 * 60 * 24 * 3 >= now
    })
    return database
}

export function assignIds(
    database: Database,
    createId: () => string = uuidv4,
): Database {
    const assignedIds = new Set<string>()
    const nextUniqueId = () => {
        let id = createId()
        while (!id || assignedIds.has(id)) id = createId()
        return id
    }
    for (const character of database.characters) {
        if (!character.chaId || assignedIds.has(character.chaId)) {
            if (character.chaId) {
                console.warn(`Duplicate chaId found: ${character.chaId}. Assigning new ID.`)
            }
            character.chaId = nextUniqueId()
        }
        assignedIds.add(character.chaId)
        for (const chat of character.chats) {
            if (!chat.id || assignedIds.has(chat.id)) {
                if (chat.id) console.warn(`Duplicate chat ID found: ${chat.id}. Assigning new ID.`)
                chat.id = nextUniqueId()
            }
            assignedIds.add(chat.id)
        }
    }
    return database
}

export function checkCharOrder(database: Database): Database {
    database.characterOrder ??= []
    const ordered = database.characterOrder.flatMap((entry) =>
        typeof entry === 'string' ? [entry] : entry?.data ?? [],
    )
    const characterIds = database.characters
        .filter((character) => !character.trashTime)
        .map((character) => character.chaId)

    for (const character of database.characters) {
        if (
            !character.trashTime &&
            character.chaId !== '§temp' &&
            character.chaId !== '§playground' &&
            !ordered.includes(character.chaId)
        ) {
            database.characterOrder.push(character.chaId)
        }
    }

    database.characterOrder = database.characterOrder
        .map((entry) => {
            if (typeof entry === 'string') return characterIds.includes(entry) ? entry : null
            if (!entry) return null
            const data = entry.data.filter((id) => characterIds.includes(id))
            return data.length > 0 ? { ...entry, data } : null
        })
        .filter((entry) => entry !== null)
    return database
}

export async function prepareDatabaseForPersistence(
    input: Database,
    options: DatabasePreparationOptions = {},
): Promise<Database> {
    const database = JSON.parse(canonicalJson(input)) as Database
    normalizeDatabaseDefaults(database)
    await checkNewFormat(database, { now: options.now })
    assignIds(database, options.createId)
    checkCharOrder(database)
    return database
}
