// @vitest-environment node
import { readFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { Window, type HTMLButtonElement, type HTMLElement } from 'happy-dom'
import { afterEach, describe, expect, it, vi } from 'vitest'

const html = readFileSync(new URL('../public/oauth/google-drive-callback.html', import.meta.url), 'utf8')
const script = html.match(/<script>([\s\S]*?)<\/script>/)![1].replace(/\r\n/g, '\n')
const windows: Window[] = []

function open(query: string, language = 'en-US') {
    const window = new Window({ url: `https://update.rsyumi.workers.dev/oauth/google-drive-callback${query}` })
    windows.push(window)
    Object.defineProperty(window.navigator, 'language', { value: language })
    window.document.write(html.replace(/<script>[\s\S]*?<\/script>/, ''))
    const copy = vi.fn().mockResolvedValue(undefined)
    Object.defineProperty(window.navigator, 'clipboard', { value: { writeText: copy } })
    new Function('window', 'document', 'navigator', script)(window, window.document, window.navigator)
    return { window, document: window.document, copy }
}

afterEach(async () => {
    await Promise.all(windows.splice(0).map(window => window.happyDOM.close()))
})

describe('standalone Google Drive callback', () => {
    it('binds its inline script to CSP and contains no external content or network permission', () => {
        const digest = createHash('sha256').update(script).digest('base64')
        expect(html).toContain(`script-src 'sha256-${digest}'`)
        expect(html).toContain("connect-src 'none'")
        expect(html).toContain('name="referrer" content="no-referrer"')
        expect(html).not.toMatch(/<(?:script|link|img)\b[^>]*(?:src|href)=/i)
    })

    it('returns only a fixed app callback and copies the HTTPS result after removing browser history query', async () => {
        const { window, document, copy } = open('?code=synthetic%2Bcode&state=synthetic-state&scope=ignored')
        const link = document.getElementById('open') as unknown as HTMLAnchorElement
        const url = new URL(link.href)
        expect(url.protocol).toBe('risunestlocal:')
        expect(url.host).toBe('oauth')
        expect(url.pathname).toBe('/google-drive')
        expect(url.searchParams.get('code')).toBe('synthetic+code')
        expect(url.searchParams.get('state')).toBe('synthetic-state')
        expect([...url.searchParams.keys()].sort()).toEqual(['code', 'state'])
        expect(window.location.search).toBe('')
        const copyButton = document.getElementById('copy') as HTMLButtonElement
        copyButton.click()
        await Promise.resolve()
        expect(copy).toHaveBeenCalledWith('https://update.rsyumi.workers.dev/oauth/google-drive-callback?state=synthetic-state&code=synthetic%2Bcode')
        expect(document.getElementById('notice')!.textContent).toContain('Copied')
    })

    it.each([
        '', '?code=synthetic', '?code=synthetic&state=a&state=b',
        '?code=a&code=b&state=s', '?code=a&error=denied&state=s',
        '?code=a&state=s#fragment', '?code=a&state=has%20space',
    ])('keeps incomplete or ambiguous callbacks inert: %s', query => {
        const { window, document } = open(query)
        expect((document.getElementById('actions') as HTMLElement).hidden).toBe(true)
        expect(document.getElementById('open')!.hasAttribute('href')).toBe(false)
        expect(window.location.search).toBe('')
    })

    it('returns denial to the same attempt without exposing provider descriptions', () => {
        const { document } = open('?error=access_denied&state=synthetic-state&error_description=%3Cscript%3E', 'ko-KR')
        const link = document.getElementById('open') as unknown as HTMLAnchorElement
        expect(new URL(link.href).searchParams.get('error')).toBe('access_denied')
        expect(document.documentElement.lang).toBe('ko')
        expect(document.getElementById('title')!.textContent).toBe('인증이 완료되지 않았습니다')
        expect(document.body.textContent).not.toContain('<script>')
    })

    it('provides a selectable manual result when clipboard permission is denied', async () => {
        const { document, copy } = open('?code=synthetic&state=synthetic-state')
        copy.mockRejectedValueOnce(new Error('synthetic denied'))
        const copyButton = document.getElementById('copy') as HTMLButtonElement
        copyButton.click()
        await Promise.resolve()
        expect((document.getElementById('manual') as HTMLElement).hidden).toBe(false)
        expect((document.getElementById('result') as unknown as HTMLTextAreaElement).value).toContain('code=synthetic')
    })
})
