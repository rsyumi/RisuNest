import '../../src/styles.css'
import { mount, unmount } from 'svelte'
import * as monaco from 'monaco-editor'
import TextEditorMonaco from '../../src/lib/Others/TextEditorMonaco.svelte'

let component: ReturnType<typeof mount> | undefined
let saved: string[] = []
let value = ''
const current = () => monaco.editor.getEditors().find(editor => editor.getDomNode()?.closest('#editor'))!

export const monacoDriver = {
    async open(language: 'markdown' | 'lua') {
        if (component) await unmount(component)
        saved = []
        value = language === 'lua' ? 'local greeting = "hello"\nprint(greeting)' : '# Synthetic title\nhello world'
        component = mount(TextEditorMonaco, {
            target: document.querySelector('#editor')!,
            props: { value, language, onchange: next => { value = next }, onsave: () => { saved.push(value) } },
        })
    },
    state: () => ({
        value: current()?.getValue(), saved,
        language: current()?.getModel()?.getLanguageId(),
        editContext: current()?.getOption(monaco.editor.EditorOption.editContext),
        languages: monaco.languages.getLanguages().map(language => language.id).sort(),
    }),
    place(text: string, lineNumber: number, column: number) {
        const editor = current()
        editor.setValue(text)
        editor.setPosition({ lineNumber, column })
        editor.revealLineInCenter(lineNumber)
        editor.focus()
    },
    moveTo(lineNumber: number, column: number) { current().setPosition({ lineNumber, column }) },
    scrollBy(pixels: number) { current().setScrollTop(current().getScrollTop() + pixels) },
    firstVisibleLine: () => current().getVisibleRanges()[0].startLineNumber,
    composition(lineNumber: number) {
        const editor = current()
        const root = editor.getDomNode()!
        const input = root.querySelector<HTMLTextAreaElement>('textarea.inputarea')!
        const style = getComputedStyle(input)
        const probe = document.body.appendChild(document.createElement('div'))
        probe.style.backgroundColor = 'var(--risu-theme-darkbg)'
        const theme = getComputedStyle(probe).backgroundColor
        probe.remove()
        const origin = root.getBoundingClientRect()
        const box = input.getBoundingClientRect()
        const lineTop = origin.top + editor.getTopForLineNumber(lineNumber) - editor.getScrollTop()
        // Where the textarea draws its caret, laid out like its own text, against the end of the composition in the line.
        const textBefore = input.value.slice(0, input.selectionEnd)
        const mirror = document.body.appendChild(document.createElement('span'))
        Object.assign(mirror.style, { position: 'absolute', whiteSpace: 'pre', fontFamily: style.fontFamily, fontSize: style.fontSize, fontWeight: style.fontWeight, letterSpacing: style.letterSpacing, tabSize: style.tabSize })
        mirror.textContent = textBefore.slice(textBefore.lastIndexOf('\n') + 1)
        const inputCaret = box.left + mirror.getBoundingClientRect().width - input.scrollLeft
        mirror.remove()
        const lineCaret = editor.getScrolledVisiblePosition(editor.getPosition()!)!
        return {
            composing: input.classList.contains('ime-input'),
            themeBackground: theme !== 'rgba(0, 0, 0, 0)' && style.backgroundColor === theme,
            fontMatchesLine: style.fontFamily === getComputedStyle(root.querySelector('.view-line span span')!).fontFamily,
            visible: style.opacity !== '0',
            onLine: Math.abs(box.top - lineTop) < 1,
            caretAligned: Math.abs(inputCaret - (origin.left + lineCaret.left)) < 2 && Math.abs(box.top - (origin.top + lineCaret.top)) < 1,
        }
    },
    // For each wrapped row but the last: the width it leaves free before the wrapping column, the width
    // of the word that starts the next row, and how far its text runs past the visible text area.
    wrapping(lineNumber: number) {
        const editor = current()
        const root = editor.getDomNode()!
        const layout = editor.getLayoutInfo()
        const origin = root.getBoundingClientRect()
        const textLeft = origin.left + layout.contentLeft
        const wrapRight = textLeft + layout.wrappingColumn * editor.getOption(monaco.editor.EditorOption.fontInfo).typicalHalfwidthCharacterWidth
        const visibleRight = textLeft + layout.contentWidth - layout.verticalScrollbarWidth
        const top = origin.top + editor.getTopForLineNumber(lineNumber) - editor.getScrollTop()
        const bottom = top + editor.getTopForLineNumber(lineNumber + 1) - editor.getTopForLineNumber(lineNumber)
        const rows = [...root.querySelectorAll<HTMLElement>('.view-line')]
            .filter(row => row.getBoundingClientRect().top >= top - 1 && row.getBoundingClientRect().top < bottom - 1)
            .sort((a, b) => a.getBoundingClientRect().top - b.getBoundingClientRect().top)
        const textBox = (row: Element, end: number) => {
            const range = document.createRange()
            const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT)
            let offset = 0
            for (let node = walker.nextNode(); node; node = walker.nextNode()) {
                if (offset === 0) range.setStart(node, 0)
                const length = node.textContent!.length
                if (end <= offset + length) { range.setEnd(node, end - offset); break }
                offset += length
            }
            return range.getBoundingClientRect()
        }
        return rows.slice(0, -1).map((row, index) => {
            const text = row.textContent!
            const next = rows[index + 1].textContent!
            const nextWordEnd = next.search(/[\s ]/)
            return {
                free: wrapRight - textBox(row, text.length).right,
                nextWord: textBox(rows[index + 1], nextWordEnd === -1 ? next.length : nextWordEnd).width,
                overflow: textBox(row, text.replace(/[\s ]+$/, '').length).right - visibleRight,
                endsWithSpace: /[\s ]$/.test(text),
            }
        })
    },
    async diff() {
        const original = monaco.editor.createModel('alpha\nbeta\ngamma', 'markdown')
        const modified = monaco.editor.createModel('alpha\nchanged\ngamma', 'markdown')
        const editor = monaco.editor.createDiffEditor(document.querySelector('#diff')!, { automaticLayout: true, editContext: false })
        try {
            await new Promise<void>(resolve => {
                const subscription = editor.onDidUpdateDiff(() => { subscription.dispose(); resolve() })
                editor.setModel({ original, modified })
            })
            return editor.getLineChanges()?.map(change => ({ original: change.originalStartLineNumber, modified: change.modifiedStartLineNumber }))
        } finally {
            editor.dispose()
            original.dispose()
            modified.dispose()
        }
    },
}

declare global { interface Window { monacoDriver: typeof monacoDriver } }
window.monacoDriver = monacoDriver
