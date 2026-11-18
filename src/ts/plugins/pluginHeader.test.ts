import { describe, expect, it } from 'vitest'
import { parsePluginHeader } from './pluginHeader'

describe('plugin header', () => {
    it('reads every declared field, with the last //@name winning', () => {
        const { header, error } = parsePluginHeader([
            '//@name first',
            '//@display-name Synthetic Plugin',
            '//@api 4.0 3.0',
            '//@version 1.2.0',
            '//@update-url https://plugins.example.test/synthetic.js',
            '//@arg limit int Max items {{min::1}}',
            '//@risu-arg label string',
            '//@link https://example.test/docs Read the docs',
            '//@allowed-ipc other-plugin',
            '//@name synthetic',
            'console.log("body")',
        ].join('\n'))

        expect(error).toBeUndefined()
        expect(header).toEqual({
            name: 'synthetic',
            displayName: 'Synthetic Plugin',
            apiVersion: '3.0',
            arguments: { limit: 'int', label: 'string' },
            argMeta: { limit: { min: '1', description: 'Max items' } },
            customLink: [{ link: 'https://example.test/docs', hoverText: 'Read the docs' }],
            updateURL: 'https://plugins.example.test/synthetic.js',
            versionOfPlugin: '1.2.0',
            allowedIPC: ['other-plugin'],
        })
    })

    it('keeps reading after the first problem and reports that one', () => {
        const { header, error } = parsePluginHeader('//@name synthetic\n//@link http://example.test\n//@arg bad float\n//@api 3.0')
        expect(error).toEqual({ message: 'plugin link must start with https, did you check it?', name: 'synthetic' })
        expect(header.apiVersion).toBe('3.0')
    })

    it.each([
        ['http://plugins.example.test/a.js', 'plugin update URL must start with https, did you put it correctly?'],
        ['not a url', 'plugin update URL is not a valid URL, did you put it correctly?'],
    ])('refuses the update URL %s', (url, message) => {
        expect(parsePluginHeader(`//@name synthetic\n//@api 3.0\n//@version 1.0.0\n//@update-url ${url}`).error?.message).toBe(message)
    })

    it('requires //@version within the first 500 bytes and at least 0.0.1', () => {
        const late = `//@name synthetic\n//@api 3.0\n//${'x'.repeat(480)}\n//@version 1.0.0`
        expect(parsePluginHeader(late).error?.message).toContain('within the first 512 Bytes')
        expect(parsePluginHeader('//@name synthetic\n//@version 1.0.0').error).toBeUndefined()
        expect(parsePluginHeader('//@name synthetic\n//@version 0.0.0').error?.message).toBe('plugin version must be at least 0.0.1')
        expect(parsePluginHeader('//@name synthetic\n//@update-url https://example.test/a.js').error?.message)
            .toBe('plugin version not found, did you put it correctly? It is required when update URL is provided.')
    })

    it.each([
        ['console.log("no header")', 'plugin name not found, did you put it correctly?'],
        ['//@name \n//@api 3.0', 'plugin name not found, did you put it correctly?'],
        ['//@name\n//@api 3.0', 'plugin name must be longer than 0, did you put it correctly?'],
        ['//@name *\n//@api 3.0', 'plugin name cannot be "*".'],
    ])('refuses a missing or reserved name in %j', (code, message) => {
        expect(parsePluginHeader(code).error?.message).toBe(message)
    })
})
