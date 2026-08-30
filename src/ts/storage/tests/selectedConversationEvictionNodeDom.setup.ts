import { Window } from 'happy-dom'

const window = new Window({ url: `file:///${process.cwd().replaceAll('\\', '/')}/index.js` })

for (const [key, value] of Object.entries({
    window,
    document: window.document,
    navigator: window.navigator,
    location: window.location,
    localStorage: window.localStorage,
    sessionStorage: window.sessionStorage,
    DOMParser: window.DOMParser,
    HTMLElement: window.HTMLElement,
})) {
    Object.defineProperty(globalThis, key, { configurable: true, value })
}
