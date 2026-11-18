import { language } from 'src/lang'
import type { PromptItem, PromptType } from './prompt'

const blockTypes: PromptType[] = [
    'plain', 'jailbreak', 'chat', 'persona', 'description', 'authornote',
    'lorebook', 'memory', 'postEverything', 'chatML', 'cache',
]

/** Prompt block types in the order the type select and the add menu list them. */
export function promptBlockTypes(includeChainOfThought: boolean): PromptType[] {
    return includeChainOfThought ? [...blockTypes, 'cot'] : blockTypes
}

export function promptBlockTypeName(type: PromptType): string {
    switch (type) {
        case 'plain': return language.formating.plain
        case 'jailbreak': return language.formating.jailbreak
        case 'chat': return language.Chat
        case 'persona': return language.formating.personaPrompt
        case 'description': return language.formating.description
        case 'authornote': return language.formating.authorNote
        case 'lorebook': return language.formating.lorebook
        case 'memory': return language.formating.memory
        case 'postEverything': return language.formating.postEverything
        case 'chatML': return 'chatML'
        case 'cache': return language.cachePoint
        case 'cot': return language.cot
        default: return ''
    }
}

/** A new block of the given type, initialized as switching an existing block to that type does. */
export function newPromptBlock(type: PromptType): PromptItem {
    switch (type) {
        case 'plain':
        case 'jailbreak':
        case 'cot':
            return { type, text: '', role: 'system', type2: 'normal' }
        case 'chatML':
            return { type, text: '' }
        case 'chat':
            return { type, rangeStart: -1000, rangeEnd: 'end' }
        case 'cache':
            return { type, name: '', depth: 1, role: 'all' }
        case 'persona':
        case 'description':
        case 'memory':
            return { type, role2: 'system' }
        case 'authornote':
            return { type, role2: 'system' }
        case 'lorebook':
        case 'postEverything':
            return { type }
    }
}
