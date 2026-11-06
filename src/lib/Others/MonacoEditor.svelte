<script lang="ts">
    import { onMount } from 'svelte';
    import * as monaco from 'monaco-editor';
    import { registerCBSMonaco } from 'src/ts/gui/codearea/cbsMonaco';
    import { registerLuaMonaco } from 'src/ts/gui/codearea/luaMonaco';
    import EditorWorker from 'monaco-editor/editor/editor.worker?worker';

    // Set up workers once globally
    if (!('MonacoEnvironment' in self)) {
        (self as any).MonacoEnvironment = {
            getWorker() {
                return new EditorWorker();
            }
        };
    }


    registerCBSMonaco()
    registerLuaMonaco()

    interface Props {
        value: string;
        language?: string;
        theme?: string;
        readonly?: boolean;
        onchange?: (value: string) => void;
        options?: monaco.editor.IStandaloneEditorConstructionOptions;
        onready?: (editor: monaco.editor.IStandaloneCodeEditor) => void;
    }

    let {
        value = $bindable(''),
        language = 'markdown',
        theme = 'vs-dark',
        readonly = false,
        onchange,
        options = {},
        onready,
    }: Props = $props();

    let container: HTMLDivElement;
    let editor: monaco.editor.IStandaloneCodeEditor;
    let compositionHidden = $state(false);

    onMount(() => {
        editor = monaco.editor.create(container, {
            value,
            language,
            theme,
            readOnly: readonly,
            editContext: false,
            automaticLayout: true,
            minimap: { enabled: false },
            wordWrap: 'on',
            // Lines render in the app font, so wrapping measures the text instead of counting fixed-width columns.
            wrappingStrategy: 'advanced',
            // Measured wrapping would otherwise split Korean words between syllables.
            wordBreak: 'keepAll',
            lineNumbers: 'on',
            scrollBeyondLastLine: false,
            fontSize: 14,
            // Lines render in the app font. Monaco measures with this font the line text it hides in front of
            // a composition, so the composition textarea lines up with the line only when they match.
            fontFamily: getComputedStyle(container).getPropertyValue('--risu-font-family').trim() || "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
            padding: { top: 8, bottom: 8 },
            renderLineHighlight: 'gutter',
            overviewRulerBorder: false,
            scrollbar: {
                verticalScrollbarSize: 6,
                horizontalScrollbarSize: 6,
            },
            ...options,
        });

        editor.onDidChangeModelContent(() => {
            const newValue = editor.getValue();
            value = newValue;
            onchange?.(newValue);
        });
        // Monaco shows a composition in its textarea, anchored to the line text from after the last tab before
        // the selection to the line end, as computed below when the composition starts. It moves the textarea
        // only while that text sits on one row and the row is in view; otherwise the textarea stays where it
        // was and shows a sliver of the line's text.
        let compositionAnchor: { lineNumber: number; fromLineStart: number; fromLineEnd: number } | null = null;
        const anchorComposition = () => {
            const input = container.querySelector<HTMLTextAreaElement>('textarea.inputarea');
            const selection = editor.getSelection();
            const model = editor.getModel();
            if (!input || !selection || !model) return;
            const textBefore = input.value.slice(0, Math.min(input.selectionStart, input.selectionEnd));
            const lineBefore = textBefore.slice(textBefore.lastIndexOf('\n') + 1);
            const lineAfter = input.value.slice(Math.max(input.selectionStart, input.selectionEnd)).split('\n', 1)[0];
            const tabAfter = lineAfter.indexOf('\t');
            const columnsAfter = model.getLineMaxColumn(selection.endLineNumber) - selection.endColumn;
            compositionAnchor = {
                lineNumber: selection.startLineNumber,
                fromLineStart: selection.startColumn - 1 - Math.min(selection.startColumn - 1, lineBefore.length - lineBefore.lastIndexOf('\t') - 1),
                fromLineEnd: columnsAfter - Math.min(columnsAfter, tabAfter === -1 ? lineAfter.length : lineAfter.length - tabAfter - 1),
            };
        };
        const trackCompositionLine = () => {
            const position = editor.getPosition();
            const visible = position && editor.getScrolledVisiblePosition(position);
            const model = editor.getModel();
            const anchor = compositionAnchor;
            const split = !!anchor && !!model && editor.getTopForPosition(anchor.lineNumber, anchor.fromLineStart + 1) !== editor.getTopForPosition(anchor.lineNumber, model.getLineMaxColumn(anchor.lineNumber) - anchor.fromLineEnd);
            compositionHidden = !visible || visible.top + visible.height <= 0 || visible.top >= editor.getLayoutInfo().height || split;
        };
        let compositionFrame: number | undefined;
        const scheduleCompositionLine = () => {
            if (compositionFrame !== undefined) return;
            // Position lookup can render synchronously; wait until Monaco finishes changing view zones.
            compositionFrame = requestAnimationFrame(() => {
                compositionFrame = undefined;
                trackCompositionLine();
            });
        };
        const scrollSubscription = editor.onDidScrollChange(scheduleCompositionLine);
        const cursorSubscription = editor.onDidChangeCursorPosition(scheduleCompositionLine);
        // Monaco has just anchored the textarea from the same textarea content and selection.
        const compositionStartSubscription = editor.onDidCompositionStart(() => {
            anchorComposition();
            scheduleCompositionLine();
        });
        const compositionEndSubscription = editor.onDidCompositionEnd(() => {
            compositionAnchor = null;
        });
        onready?.(editor);

        return () => {
            scrollSubscription.dispose();
            cursorSubscription.dispose();
            compositionStartSubscription.dispose();
            compositionEndSubscription.dispose();
            if (compositionFrame !== undefined) cancelAnimationFrame(compositionFrame);
            editor.dispose();
        };
    });

    // Sync external value changes into editor without triggering onDidChangeModelContent loop
    $effect(() => {
        if (editor && editor.getValue() !== value) {
            const model = editor.getModel();
            if (model) {
                model.pushEditOperations(
                    [],
                    [{ range: model.getFullModelRange(), text: value }],
                    () => null
                );
            }
        }
    });
</script>

<div bind:this={container} class="w-full h-full" class:composition-hidden={compositionHidden}></div>

<style>
    /* Monaco sets the composition textarea font inline with its own fallback fonts; use exactly the lines' font. */
    div :global(.monaco-editor .inputarea.ime-input) {
        font-family: var(--risu-font-family) !important;
    }
    .composition-hidden :global(.monaco-editor .inputarea.ime-input) {
        opacity: 0;
    }
</style>
