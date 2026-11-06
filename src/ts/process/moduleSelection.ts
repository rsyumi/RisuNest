import type { RisuModule } from './modules'

export interface ModuleSelectionSource {
    modules?: RisuModule[]
    enabledModules?: string[]
    moduleIntergration?: string
}

function deduplicateModuleById(modules: RisuModule[]) {
    let ids: string[] = []
    let newModules: RisuModule[] = []
    for (let i = 0; i < modules.length; i++) {
        if (ids.includes(modules[i].id)) {
            continue
        }
        ids.push(modules[i].id)
        newModules.push(modules[i])
    }
    return newModules
}

function moduleIntegrationKeys(moduleIntergration: string | undefined): string[] {
    return moduleIntergration ? moduleIntergration.split(',').map((s) => s.trim()) : []
}

/** Whether a list of module references names the module, by ID or by namespace. */
function namesModule(ids: { has(value: string): boolean }, module: { id: string; namespace?: string }): boolean {
    return ids.has(module.id) || (!!module.namespace && ids.has(module.namespace))
}

/** The modules a prompt for one conversation uses, from explicit inputs instead of the selected chat. */
export function selectConversationModules(
    database: ModuleSelectionSource,
    chatModules: string[] | undefined,
    characterModules: string[] | undefined,
    boundPersona: { embeddedModule?: RisuModule } | null | undefined,
): RisuModule[] {
    let ids = database.enabledModules ?? []
    ids = ids.concat(chatModules ?? [])
    if (characterModules) {
        ids = ids.concat(characterModules)
    }
    ids = ids.concat(moduleIntegrationKeys(database.moduleIntergration))
    const idSet = new Set(ids)
    const modules = deduplicateModuleById((database.modules ?? []).filter(m => namesModule(idSet, m)))
    if (boundPersona?.embeddedModule) {
        modules.push(boundPersona.embeddedModule)
    }
    return deduplicateModuleById(modules)
}

export type ModuleScope = 'global' | 'character' | 'chat'

export interface ModuleScopeSources {
    enabledModules?: readonly string[]
    moduleIntergration?: string
    characterModules?: readonly string[]
    chatModules?: readonly string[]
}

export interface ModuleScopeState {
    global: boolean
    character: boolean
    chat: boolean
    /** Enabled through the module integration setting. */
    integrated: boolean
    /** Used by the prompt of the current chat, through any scope or the integration setting. */
    applied: boolean
}

/** Which scopes list the module by ID, and whether `selectConversationModules` would pick it. */
export function moduleScopeState(module: { id: string; namespace?: string }, sources: ModuleScopeSources): ModuleScopeState {
    const integrated = namesModule(new Set(moduleIntegrationKeys(sources.moduleIntergration)), module)
    const scoped = new Set([...(sources.enabledModules ?? []), ...(sources.characterModules ?? []), ...(sources.chatModules ?? [])])
    return {
        global: sources.enabledModules?.includes(module.id) ?? false,
        character: sources.characterModules?.includes(module.id) ?? false,
        chat: sources.chatModules?.includes(module.id) ?? false,
        integrated,
        applied: integrated || namesModule(scoped, module),
    }
}

/** The list with `id` added once, or with every occurrence of it removed. */
export function withModule(list: readonly string[] | undefined, id: string, enabled: boolean): string[] {
    if (enabled) return list?.includes(id) ? [...list] : [...(list ?? []), id]
    return (list ?? []).filter((value) => value !== id)
}
