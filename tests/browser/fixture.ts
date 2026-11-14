import { readFileSync } from 'node:fs'
import { test as base, expect } from '@playwright/test'
import { classifyTestRequest } from '../support/testNetwork'
import type { BrowserDriver } from './main'
import type { thoughtDriver } from './thoughtDriver'
import type { settledThought } from './settledThoughtMain'
declare global { interface Window { boundary: BrowserDriver; thought: typeof thoughtDriver; settledThought: typeof settledThought } }
export const test = base.extend<{ guarded: void }>({
    guarded: [async ({ context, baseURL }, use) => {
        const rejected: string[] = []
        const origins = new Set([new URL(baseURL!).origin])
        await context.route('**/*', async route => {
            const reason = classifyTestRequest(route.request().url(), origins)
            // Serve our owned HTML directly so machine-level HTTP injectors cannot add scripts.
            const pathname = new URL(route.request().url()).pathname
            if (!reason && (pathname === '/' || pathname === '/imageFormats.html' || pathname === '/sidebarTouch.html' || pathname === '/dragDrop.html' || pathname === '/chatList.html' || pathname === '/modalNavigation.html' || pathname === '/lazyApp.html' || pathname === '/monaco.html' || pathname === '/settledThought.html' || pathname === '/switchContainment.html' || pathname === '/chatRendering.html')) {
                const file = pathname === '/' ? 'index.html' : pathname.slice(1)
                await route.fulfill({ contentType: 'text/html', body: readFileSync(new URL(`../../.tmp/test-results/browser/dist/${file}`, import.meta.url), 'utf8') })
                return
            }
            if (reason) { rejected.push(`${reason}: ${route.request().url()}`); await route.abort() } else await route.continue()
        })
        await use()
        expect(rejected).toEqual([])
    }, { auto: true }],
})
export { expect }
