import { describe, expect, it, vi } from 'vitest'
import { parse } from 'svelte/compiler'
import source from './UserSettings.svelte?raw'

function handler(name: string) {
    let expression: any
    function visit(node: any) {
        if (!node || typeof node !== 'object') return
        if (node.type === 'Attribute' && node.name === name) {
            const candidate = node.value?.[0]?.expression
            if (
                candidate &&
                source.slice(candidate.start, candidate.end).includes('unMigrationAccount')
            ) {
                expression = candidate
            }
        }
        for (const value of Object.values(node)) {
            if (Array.isArray(value)) value.forEach(visit)
            else if (value && typeof value === 'object') visit(value)
        }
    }
    visit(parse(source).html)
    if (!expression) throw new Error(`Missing account ${name} handler`)
    return (dependencies: Record<string, unknown>) =>
        new Function(
            ...Object.keys(dependencies),
            `return (${source.slice(expression.start, expression.end)})`,
        )(...Object.values(dependencies))
}

function setup() {
    const account = { token: 'synthetic', useSync: true }
    const dependencies = {
        isTauri: false,
        nativeAccountBusy: false,
        $accountUnmigrationBusy: false,
        DBState: { db: { account: account as typeof account | undefined } },
        forageStorage: { isAccount: true },
        unMigrationAccount: vi.fn(),
        alertError: vi.fn(),
        language: { accountUnmigration: { failed: 'Local transition failed. Please retry.' } },
    }
    return { account, dependencies }
}

describe('web account settings actions', () => {
    it('awaits logout materialization without deleting the live account, and reports failure', async () => {
        const { account, dependencies } = setup()
        let reject!: (error: Error) => void
        dependencies.unMigrationAccount.mockImplementation(
            () =>
                new Promise((_resolve, fail) => {
                    reject = fail
                }),
        )
        let settled = false
        const operation = handler('onclick')(dependencies)().finally(() => {
            settled = true
        })
        await Promise.resolve()
        expect(dependencies.DBState.db.account).toBe(account)
        expect(settled).toBe(false)
        reject(new Error('snapshot changed'))
        await operation
        expect(dependencies.DBState.db.account).toBe(account)
        expect(dependencies.alertError).toHaveBeenCalledOnce()
    })

    it('starts unmigration when the checkbox changes to unchecked and reports failure', async () => {
        const { account, dependencies } = setup()
        dependencies.unMigrationAccount.mockRejectedValue(new Error('download failed'))
        await handler('onChange')(dependencies)(false)
        expect(dependencies.unMigrationAccount).toHaveBeenCalledOnce()
        expect(dependencies.DBState.db.account).toBe(account)
        expect(dependencies.alertError).toHaveBeenCalledOnce()
    })

    it('does not start a duplicate logout while unmigration is busy', async () => {
        const { dependencies } = setup()
        dependencies.$accountUnmigrationBusy = true
        await handler('onclick')(dependencies)()
        expect(dependencies.unMigrationAccount).not.toHaveBeenCalled()
        expect(dependencies.DBState.db.account).toBeDefined()
    })
})
