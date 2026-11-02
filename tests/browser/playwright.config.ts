import { spawnSync } from 'node:child_process'
import { defineConfig } from '@playwright/test'

// Windows can reserve any fixed port, so each run takes a free one and its workers inherit it.
function freeLoopbackPort() {
    const probe = spawnSync(process.execPath, ['-e', "const server = require('node:net').createServer(); server.listen(0, '127.0.0.1', () => { process.stdout.write(String(server.address().port)); server.close() })"], { encoding: 'utf8', windowsHide: true })
    const port = Number(probe.stdout)
    if (probe.error || probe.status !== 0 || !Number.isInteger(port) || port <= 0) throw new Error(`No free loopback port: ${probe.error ?? probe.stderr}`)
    return port
}
process.env.RISUNEST_BROWSER_PORT ??= String(freeLoopbackPort())
const origin = `http://127.0.0.1:${process.env.RISUNEST_BROWSER_PORT}`

export default defineConfig({
    testDir: '.', testMatch: '**/*.browser.spec.ts', workers: 1, retries: 0,
    outputDir: '../../.tmp/test-results/browser/artifacts',
    projects: [
        // gc() lets a test collect a plugin frame's dropped remote references.
        { name: 'chromium', use: { browserName: 'chromium', launchOptions: { args: ['--js-flags=--expose-gc'] } } },
        { name: 'webkit', use: { browserName: 'webkit' }, testMatch: ['**/pluginIframe.browser.spec.ts', '**/largeThoughtExpansion.browser.spec.ts', '**/modalNavigation.browser.spec.ts', '**/lazyApp.browser.spec.ts', '**/monaco.browser.spec.ts', '**/switchContainment.browser.spec.ts', '**/chatRendering.browser.spec.ts', '**/youtubeEmbed.browser.spec.ts'] },
    ],
    use: { headless: true, baseURL: origin, serviceWorkers: 'block' },
    webServer: { command: `pnpm exec vite build --mode agent --config vite.config.ts && pnpm exec vite preview --mode agent --config vite.config.ts --host 127.0.0.1 --port ${process.env.RISUNEST_BROWSER_PORT} --strictPort`, url: origin, reuseExistingServer: false },
})
