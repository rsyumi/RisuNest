import { ParseMarkdown } from '../../src/ts/parser/parser.svelte'

const fixture = [
    '```javascript',
    'const answer = Array.from({ length: 128 }, (_, index) => index).reduce((sum, value) => sum + value, 0)',
    'console.log(answer)',
    '```',
].join('\n')

Object.assign(globalThis, {
    __risuW8HighlightReady: true,
    __risuW8MeasureHighlight: async () => {
        performance.clearResourceTimings()
        const startedAt = performance.now()
        const output = await ParseMarkdown(fixture, null, 'normal')
        const endedAt = performance.now()
        return {
            durationMs: endedAt - startedAt,
            validOutput: output.includes('class="hljs"') && output.includes('hljs-keyword'),
            loadedResources: performance.getEntriesByType('resource').map((entry) => entry.name),
        }
    },
})
