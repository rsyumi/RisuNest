import { describe, expect, it, vi } from 'vitest'
import { parse } from 'svelte/compiler'
import { readFileSync } from 'node:fs'
const source = readFileSync('src/lib/Setting/Pages/UserSettings.svelte', 'utf8')

function handler(name: string, marker = 'unMigrationAccount') {
    let expression: any
    function visit(node: any) {
        if (!node || typeof node !== 'object') return
        if (node.type === 'Attribute' && node.name === name) {
            const candidate = node.value?.[0]?.expression
            if (
                candidate &&
                source.slice(candidate.start, candidate.end).includes(marker)
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

describe('native account settings actions', () => {
    const failureText = "Couldn't complete the backup operation."

    function nativeDependencies() {
        const account = { id: 'account-a', token: 'synthetic', data: {} }
        const flow = {
            login: vi.fn(),
            logout: vi.fn(),
        }
        return {
            account,
            flow,
            dependencies: {
                hubURL: 'https://example.invalid',
                openIframeURL: 'https://example.invalid/hub/login',
                drivePopup: { source: null, close: vi.fn() },
                accountIframe: undefined,
                resolveExpectedOfficialAccountMessageUrl: vi.fn(() => 'https://example.invalid'),
                isExpectedHubMessage: vi.fn(() => true),
                isTauri: true,
                nativeAccountBusy: false,
                $accountUnmigrationBusy: false,
                runNativeAccountOperation: (operation: () => Promise<unknown>) => operation(),
                getNativeOfficialAccountFlow: () => flow,
                alertError: vi.fn(),
                language: { risuNest: { backup: { actionFailed: failureText } } },
                DBState: { db: { account: account as typeof account | undefined } },
                forageStorage: { isAccount: false },
                unMigrationAccount: vi.fn(),
                loadRisuAccountData: vi.fn(),
                saveRisuAccountData: vi.fn(),
            },
        }
    }

    it('keeps the login session unchanged and reports a native login failure', async () => {
        const { dependencies, flow } = nativeDependencies()
        dependencies.DBState.db.account = undefined
        flow.login.mockRejectedValue(new Error('app kv failed'))

        await handler('onmessage', 'getNativeOfficialAccountFlow().login')(dependencies)({
            data: {
                msg: {
                    id: 'account-a',
                    token: 'synthetic',
                    data: { vaild: true },
                },
            },
        })

        expect(dependencies.DBState.db.account).toBeUndefined()
        expect(dependencies.alertError).toHaveBeenCalledWith(failureText)
    })

    it('keeps the visible account and reports a native logout failure', async () => {
        const { account, dependencies, flow } = nativeDependencies()
        flow.logout.mockRejectedValue(new Error('metadata flush failed'))

        await handler('onclick', 'getNativeOfficialAccountFlow().logout')(dependencies)()

        expect(dependencies.DBState.db.account).toBe(account)
        expect(dependencies.alertError).toHaveBeenCalledWith(failureText)
    })
})
