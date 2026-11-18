import { getContext, setContext } from 'svelte'

export interface TextEditorPopupRequest {
    value: string
    title?: string
    /** Monaco language of the text on desktop. Defaults to markdown. */
    language?: string
    /** Offers the CBS preview with its token count and toggles. */
    preview?: boolean
    /** Previews with the preset the settings window edits instead of the applied preset. */
    presetEditor?: boolean
    /** Applies the edited text. Resolving `false` keeps the editor open with the draft. */
    save: (value: string) => boolean | Promise<boolean>
    /** Receives the draft whenever the reader changes it. */
    input?: (value: string) => void
    /** Runs when the editor closes without saving. */
    cancel?: () => void
}

class TextEditorPopupState {
    request = $state.raw<TextEditorPopupRequest | null>(null)
}

export const textEditorPopup = new TextEditorPopupState()

export function openTextEditorPopup(request: TextEditorPopupRequest): void {
    const previous = textEditorPopup.request
    textEditorPopup.request = request
    if (previous && previous !== request) previous.cancel?.()
}

/** Closes the editor if `request` is still the one it shows. */
export function closeTextEditorPopup(request: TextEditorPopupRequest): void {
    if (textEditorPopup.request === request) textEditorPopup.request = null
}

/** Closes the editor without saving if `request` is still the one it shows. */
export function cancelTextEditorPopup(request: TextEditorPopupRequest): void {
    if (textEditorPopup.request !== request) return
    textEditorPopup.request = null
    request.cancel?.()
}

const insideKey = Symbol('textEditorPopup')

/** Marks the components below as content of the editor, which cannot open a second editor over itself. */
export function markTextEditorPopupContent(): void {
    setContext(insideKey, true)
}

export function isInsideTextEditorPopup(): boolean {
    return getContext<boolean | undefined>(insideKey) === true
}
