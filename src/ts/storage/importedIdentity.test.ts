import { describe, expect, it } from 'vitest'
import type { Database } from './database.svelte'
import { prepareUpstreamImport } from './importedIdentity'

describe('upstream import identity preparation', () => {
    it('preserves explicit root mirrors before defaults and leaves the source unchanged', () => {
        const source = {
            botPresets: [{ name:'preset', mainPrompt:'old' }], botPresetsId:0,
            personas:[{ name:'old', icon:'old' }], selectedPersona:0,
            mainPrompt:'explicit prompt', username:'explicit name', userIcon:'explicit icon',
            globalChatVariables:{ toggle_example:'1' },
        } as unknown as Database
        const prepared = prepareUpstreamImport(source)
        expect(prepared.botPresets[0].mainPrompt).toBe('explicit prompt')
        expect(prepared.personas[0]).toMatchObject({ name:'explicit name', icon:'explicit icon' })
        expect(prepared.explicitGlobalChatVariables).toEqual({ toggle_example:'1' })
        expect(prepared.botPresets[0].id).toMatch(/^[0-9a-f-]{36}$/)
        expect(prepared.personas[0].id).toMatch(/^[0-9a-f-]{36}$/)
        expect(source.botPresets[0]).toEqual({name:'preset', mainPrompt:'old'})
        expect(Object.hasOwn(prepared,'temperature')).toBe(false)
    })
    it('assigns only missing or duplicate imported record IDs and preserves message references', () => {
        const message={chatId:'message-id', data:'synthetic'}
        const source={botPresets:[],globalChatVariables:{},
            personas:[{id:'a',embeddedModule:{id:7}},{id:'b',embeddedModule:{name:'m'}},{id:'c',embeddedModule:{id:'kept'}}],
            modules:[{id:'module'},{id:'module'}],loadouts:[{id:'loadout'}],customModels:[{}],
            characters:[{chaId:'card-assigned', chats:[{id:'conversation',message:[message],bookmarks:['message-id'],hypaV3Data:{memos:['message-id']}}]}],
        } as unknown as Database
        const prepared=prepareUpstreamImport(source)
        expect(prepared.modules[0].id).toBe('module')
        expect(prepared.modules[1].id).not.toBe('module')
        expect(prepared.loadouts[0].id).toBe('loadout')
        expect(prepared.customModels[0].id).toMatch(/^[0-9a-f-]{36}$/)
        expect(prepared.personas.map(persona=>persona.embeddedModule?.id)).toEqual([
            expect.stringMatching(/^[0-9a-f-]{36}$/), expect.stringMatching(/^[0-9a-f-]{36}$/), 'kept',
        ])
        expect(prepared.characters).toEqual(source.characters)
    })
})
