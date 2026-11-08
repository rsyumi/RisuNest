// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import DOMPurify from 'dompurify'
import { writable } from 'svelte/store'
import { ParseMarkdown, parseMarkdownSafe } from '../parser.svelte'

vi.unmock('katex')
vi.mock('../../platform', async () => (await import('../../platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('../../storage/database.svelte', () => ({
    appVer: 'synthetic',
    getCurrentCharacter: () => ({}),
    getDatabase: () => ({}),
}))
vi.mock('../../globalApi.svelte', () => ({
    aiWatermarkingLawApplies: () => false,
    getFileSrc: async () => '',
}))
vi.mock('../../stores.svelte', () => ({
    DBState: { db: { characters: [], globalChatVariables: {}, templateDefaultVariables: '' } },
    selIdState: { selId: -1 },
    selectedCharID: writable(-1),
}))

beforeEach(() => expect(DOMPurify.isSupported).toBe(true))
afterEach(() => vi.restoreAllMocks())

const render = (source: string) => new DOMParser()
    .parseFromString(parseMarkdownSafe(source), 'text/html').body

describe('safe Markdown with the real KaTeX renderer', () => {
    it('preserves a rendered fraction through Markdown and sanitization', () => {
        const body = render(String.raw`Before $$\frac{1}{2}$$ after`)
        const fraction = body.querySelector('math mfrac')
        expect(fraction, body.innerHTML).not.toBeNull()
        expect([...fraction!.querySelectorAll('mn')].map((node) => node.textContent)).toEqual(['1', '2'])
        expect(body.textContent).toContain('Before')
        expect(body.textContent).toContain('after')
        expect(fraction!.namespaceURI).toBe('http://www.w3.org/1998/Math/MathML')
    })

    it('keeps malformed math as readable source without breaking adjacent Markdown', () => {
        const error = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const body = render(String.raw`**Before** $$\notARealCommand{x}$$ after`)
        expect(body.querySelector('math')).toBeNull()
        expect(body.querySelector('strong')?.textContent).toBe('Before')
        expect(body.textContent).toContain(String.raw`$$\notARealCommand{x}$$`)
        expect(body.textContent).toContain('after')
        expect(error).toHaveBeenCalledOnce()
        expect(error.mock.calls[0][0]).toBe('KaTeX render error:')
    })

    it('retains ordinary Markdown while removing executable HTML and link attributes', () => {
        const body = render('**Safe text** and `code` <script>throw new Error("untrusted")</script><a href="javascript:alert(1)" style="color:red" class="untrusted">label</a>')
        expect(body.querySelector('strong')?.textContent).toBe('Safe text')
        expect(body.querySelector('code')?.textContent).toBe('code')
        expect(body.textContent).toContain('label')
        expect(body.querySelector('script, a, [href], [style], [class]')).toBeNull()
        expect(body.textContent).not.toContain('untrusted')
    })

    it('keeps dollar sequences literal inside fenced code', async () => {
        for (const fence of ['bash', 'unknown-language']) {
            const body = new DOMParser().parseFromString(
                await ParseMarkdown(`\`\`\`${fence}\necho $$ $& $' done\n\`\`\``, null, 'normal'),
                'text/html',
            ).body
            expect(body.querySelector('pre code')?.textContent).toBe("echo $$ $& $' done\n")
        }
    })

    it('drops link targets that cannot open a page instead of leaving an empty href to the app', async () => {
        const body = new DOMParser().parseFromString(
            await ParseMarkdown('<Lacuna:Thoughts> <a href="#top">top</a> <a href="mailto:user@synthetic.invalid">mail</a> <a href="https://synthetic.invalid/page">web</a>', null, 'normal'),
            'text/html',
        ).body
        const links = [...body.querySelectorAll('a')]
        expect(links.map((link) => link.textContent)).toEqual(['Lacuna:Thoughts', 'top', 'mail', 'web'])
        expect(links.slice(0, 3).map((link) => link.hasAttribute('href'))).toEqual([false, false, false])
        expect([links[3].getAttribute('href'), links[3].getAttribute('target')]).toEqual(['https://synthetic.invalid/page', '_blank'])
    })
})
