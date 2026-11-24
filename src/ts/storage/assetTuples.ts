import type { WorkingSetCommit } from './persistentDataStore'

const lacksExtension = (asset: unknown): asset is unknown[] =>
    Array.isArray(asset) && typeof asset[0] === 'string' && typeof asset[1] === 'string' && asset[2] == null

/** `list` with `change` applied to each entry, or `list` itself when no entry changes. */
function mapChanged<T>(list: T, change: (entry: unknown) => unknown): T {
    if (!Array.isArray(list)) return list
    let result: unknown[] | undefined
    list.forEach((entry, index) => {
        const next = change(entry)
        if (next !== entry) (result ??= [...list])[index] = next
    })
    return (result ?? list) as T
}

/** `record` with `change` applied to its `key`, or `record` itself when the value stays. */
function withField<T>(record: T, key: string, change: (value: unknown) => unknown): T {
    if (!record || typeof record !== 'object' || !Object.hasOwn(record, key)) return record
    const value = (record as Record<string, unknown>)[key]
    const next = change(value)
    return next === value ? record : { ...record, [key]: next }
}

const withExtensions = (assets: unknown) =>
    mapChanged(assets, (asset) => lacksExtension(asset) ? [asset[0], asset[1], '', ...asset.slice(3)] : asset)
const moduleWithExtensions = (module: unknown) => withField(module, 'assets', withExtensions)
const personaWithExtensions = (persona: unknown) => withField(persona, 'embeddedModule', moduleWithExtensions)
const modulesWithExtensions = (modules: unknown) => mapChanged(modules, moduleWithExtensions)
const personasWithExtensions = (personas: unknown) => mapChanged(personas, personaWithExtensions)
const characterWithExtensions = <T>(character: T): T => withField(character, 'additionalAssets', withExtensions)

type Change = (value: unknown) => unknown
const collections = new Map<unknown, Change>([['modules', modulesWithExtensions], ['personas', personasWithExtensions]])
const records = new Map<unknown, Change>([['modules', moduleWithExtensions], ['personas', personaWithExtensions]])
const ownerUnit = /"(?:additionalAssets|modules|personas|embeddedModule)"/

/** The change for the value of the unit `key` names, when that unit holds asset tuples. */
function unitChange(key: string): Change | undefined {
    if (!ownerUnit.test(key)) return undefined
    const parts: unknown = JSON.parse(key)
    if (!Array.isArray(parts)) return undefined
    const [kind, second, third] = parts
    if (parts.length === 2 && kind === 'root') return collections.get(second)
    if (parts.length !== 3) return undefined
    if (kind === 'character' && third === 'additionalAssets') return withExtensions
    if (kind === 'persona' && third === 'embeddedModule') return moduleWithExtensions
    return kind === 'record' ? records.get(second) : undefined
}

/**
 * `commit` with an empty extension in every `[name, key]` and `[name, key, null]` asset tuple it
 * carries, or `commit` itself when it carries none. RisuAI wrote assets without an extension
 * before it stored one, and a card or module file can carry `null` there. The native store takes
 * three strings and refuses the whole commit otherwise.
 */
export function withCommitAssetExtensions<T extends Omit<WorkingSetCommit, 'assetAliases'>>(commit: T): T {
    let result = commit
    for (const key of ['character', 'replaceCharacter', 'addCharacter'] as const) {
        result = withField(result, key, characterWithExtensions)
    }
    result = withField(result, 'characterDetails', (details) => mapChanged(details, characterWithExtensions))
    result = withField(result, 'root', (root) =>
        withField(withField(root, 'modules', modulesWithExtensions), 'personas', personasWithExtensions))
    const mutationsWith = (find: (key: string) => Change | undefined): Change => (mutations) =>
        mapChanged(mutations, (mutation) => {
            const { type, key } = mutation as { type: string; key: string }
            const change = type === 'set' ? find(key) : undefined
            return change ? withField(mutation, 'value', change) : mutation
        })
    result = withField(result, 'rootMutations', mutationsWith((key) => collections.get(key)))
    return withField(result, 'unitMutations', mutationsWith(unitChange))
}
