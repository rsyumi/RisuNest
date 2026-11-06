import { describe, expect, it } from 'vitest'
import type { RisuModule } from './modules'
import { moduleScopeState, selectConversationModules, withModule, type ModuleScopeSources } from './moduleSelection'

const module = (id: string, namespace?: string) => ({ id, name: id, description: '', namespace }) as RisuModule

describe('module scope state', () => {
    it('reports each scope by ID and marks a module applied exactly when the prompt selects it', () => {
        const modules = [module('global'), module('character'), module('chat'), module('named', 'shared'), module('integrated', 'tools'), module('off')]
        const sources = {
            enabledModules: ['global'],
            characterModules: ['character', 'shared'],
            chatModules: ['chat'],
            moduleIntergration: 'missing, tools ',
        } satisfies ModuleScopeSources
        const selected = selectConversationModules(
            { modules, enabledModules: sources.enabledModules, moduleIntergration: sources.moduleIntergration },
            sources.chatModules,
            sources.characterModules,
            null,
        ).map((item) => item.id)

        const states = Object.fromEntries(modules.map((item) => [item.id, moduleScopeState(item, sources)]))
        expect(modules.filter((item) => states[item.id].applied).map((item) => item.id)).toEqual(selected)
        expect(states.global).toMatchObject({ global: true, character: false, chat: false, integrated: false })
        expect(states.character).toMatchObject({ global: false, character: true, chat: false })
        expect(states.chat).toMatchObject({ global: false, character: false, chat: true })
        // A namespace reference applies the module without pressing a scope button for it.
        expect(states.named).toMatchObject({ character: false, applied: true, integrated: false })
        expect(states.integrated).toMatchObject({ global: false, character: false, chat: false, integrated: true, applied: true })
        expect(states.off).toMatchObject({ global: false, character: false, chat: false, integrated: false, applied: false })
    })

    it('treats missing lists as empty', () => {
        expect(moduleScopeState(module('a'), {})).toEqual({ global: false, character: false, chat: false, integrated: false, applied: false })
    })
})

describe('withModule', () => {
    it('adds a module once and removes every occurrence without changing the input', () => {
        const stored = ['a', 'b', 'a']
        expect(withModule(undefined, 'a', true)).toEqual(['a'])
        expect(withModule(stored, 'c', true)).toEqual(['a', 'b', 'a', 'c'])
        expect(withModule(stored, 'b', true)).toEqual(['a', 'b', 'a'])
        expect(withModule(stored, 'a', false)).toEqual(['b'])
        expect(withModule(undefined, 'a', false)).toEqual([])
        expect(stored).toEqual(['a', 'b', 'a'])
    })
})
