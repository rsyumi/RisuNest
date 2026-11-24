import { readFileSync } from 'node:fs'
import ts from 'typescript'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'
// stores.svelte loads before database.svelte, as the app loads them.
import '../stores.svelte'
import { normalizeDatabaseDefaults } from '../storage/database.svelte'
import { sharedRootFields } from '../storage/persistentRootFields'
import { jsonKindOf } from './risunestHostContext'
import { risunestSettingKeyClasses } from './risunestSettingKeys'

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())

/** Declared without null, but cleared with null by their settings toggle. */
const nullClearedKeys = ['textScreenColor', 'textScreenBorder']

function offeredKeys() {
    return [...risunestSettingKeyClasses].flatMap(([key, entry]) => entry.access === 'excluded' ? [] : [[key, entry] as const])
}

/** Database root keys whose declared type names null. */
function declaredNullableKeys(): string[] {
    const path = 'src/ts/storage/database.svelte.ts'
    const source = ts.createSourceFile(path, readFileSync(path, 'utf8'), ts.ScriptTarget.Latest, true)
    const declaration = source.statements.find((statement): statement is ts.InterfaceDeclaration =>
        ts.isInterfaceDeclaration(statement) && statement.name.text === 'Database')
    if (!declaration) throw new Error('Database interface not found')
    return declaration.members.flatMap((member) => {
        if (!ts.isPropertySignature(member) || !member.type || !ts.isUnionTypeNode(member.type)) return []
        const nullable = member.type.types.some((type) => ts.isLiteralTypeNode(type) && type.literal.kind === ts.SyntaxKind.NullKeyword)
        return nullable ? [(member.name as ts.Identifier).text] : []
    })
}

describe('settings key classification', () => {
    it('classifies exactly the shared root fields', () => {
        const classified = [...risunestSettingKeyClasses.keys()]
        expect([...sharedRootFields].filter((key) => !risunestSettingKeyClasses.has(key))).toEqual([])
        expect(classified.filter((key) => !sharedRootFields.has(key))).toEqual([])
    })

    it('gives each offered key the kind of the default the app loads it with', () => {
        const defaults = normalizeDatabaseDefaults({} as Database) as unknown as Record<string, unknown>
        const mismatches = offeredKeys().flatMap(([key, entry]) => {
            const value = defaults[key]
            if (value === undefined) return []
            if (value === null ? entry.nullable : jsonKindOf(value) === entry.type) return []
            return [{ key, type: entry.type, nullable: entry.nullable, default: value }]
        })
        expect(mismatches).toEqual([])
    })

    it('allows null exactly where the declared type or the settings screen stores it', () => {
        const declared = declaredNullableKeys()
        const offered = offeredKeys()
        expect(offered.filter(([, entry]) => entry.nullable).map(([key]) => key).sort())
            .toEqual(offered.map(([key]) => key).filter((key) => declared.includes(key) || nullClearedKeys.includes(key)).sort())
    })
})
