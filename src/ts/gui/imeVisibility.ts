const KEYBOARD_GAP_PX = 8
const PLUGIN_FRAME_SELECTOR = 'iframe[data-risu-plugin-frame]'
const PLUGIN_INPUT_FOCUS_MESSAGE_TYPE = 'RISU_PLUGIN_INPUT_FOCUS'
const NON_TEXT_INPUT_TYPES = new Set([
    'button',
    'checkbox',
    'color',
    'file',
    'hidden',
    'image',
    'radio',
    'range',
    'reset',
    'submit',
])

function isTextEntry(element: Element | null) {
    if (element instanceof HTMLTextAreaElement) {
        return !element.disabled && !element.readOnly
    }
    if (element instanceof HTMLInputElement) {
        return !element.disabled && !element.readOnly && !NON_TEXT_INPUT_TYPES.has(element.type)
    }
    return element instanceof HTMLElement && element.isContentEditable
}

type InputBounds = {
    top: number
    bottom: number
}

function readInputBounds(value: unknown): InputBounds | null {
    if (!value || typeof value !== 'object') {
        return null
    }

    const { top, bottom } = value as Partial<InputBounds>
    if (
        typeof top !== 'number' ||
        typeof bottom !== 'number' ||
        !Number.isFinite(top) ||
        !Number.isFinite(bottom) ||
        bottom < top
    ) {
        return null
    }

    return { top, bottom }
}

function findPluginFrame(source: MessageEventSource | null) {
    return Array.from(document.querySelectorAll<HTMLIFrameElement>(PLUGIN_FRAME_SELECTOR))
        .find((frame) => frame.contentWindow === source) ?? null
}

export function keepFocusedInputVisible(node: HTMLElement, enabled: boolean) {
    const viewport = window.visualViewport
    if (!enabled || !viewport) {
        return { destroy() {} }
    }

    let pluginInput: { frame: HTMLIFrameElement; bounds: InputBounds } | null = null
    let shiftedTarget: HTMLElement | null = null
    let initialTranslate = ''
    let appliedShift = 0
    let animationFrame: number | null = null

    const restoreShiftedTarget = () => {
        if (shiftedTarget) {
            shiftedTarget.style.translate = initialTranslate
        }
        shiftedTarget = null
        initialTranslate = ''
        appliedShift = 0
    }

    const setShift = (target: HTMLElement | null, shift: number) => {
        if (shiftedTarget !== target) {
            restoreShiftedTarget()
            shiftedTarget = target
            initialTranslate = target?.style.translate ?? ''
        }
        if (!target) {
            return
        }

        appliedShift = shift
        target.style.translate = shift > 0 ? `0 -${shift}px` : initialTranslate
    }

    const getFocusedInput = () => {
        if (
            pluginInput &&
            pluginInput.frame.isConnected &&
            document.activeElement === pluginInput.frame
        ) {
            const frameRect = pluginInput.frame.getBoundingClientRect()
            const currentShift = shiftedTarget === pluginInput.frame ? appliedShift : 0
            const frameTop = frameRect.top + currentShift
            return {
                target: pluginInput.frame,
                top: frameTop + pluginInput.bounds.top,
                bottom: frameTop + pluginInput.bounds.bottom,
            }
        }

        const activeElement = document.activeElement
        if (!isTextEntry(activeElement) || !node.contains(activeElement)) {
            return null
        }

        const inputRect = activeElement.getBoundingClientRect()
        const currentShift = shiftedTarget === node ? appliedShift : 0
        return {
            target: node,
            top: inputRect.top + currentShift,
            bottom: inputRect.bottom + currentShift,
        }
    }

    const update = () => {
        animationFrame = null
        const focusedInput = getFocusedInput()
        if (!focusedInput) {
            setShift(null, 0)
            return
        }

        const visualTop = viewport.offsetTop
        const visualBottom = viewport.offsetTop + viewport.height
        const overlap = focusedInput.bottom - visualBottom
        const requiredShift = overlap > 0 ? Math.ceil(overlap + KEYBOARD_GAP_PX) : 0
        const maximumShift = Math.max(0, Math.floor(focusedInput.top - visualTop))
        setShift(focusedInput.target, Math.min(requiredShift, maximumShift))
    }

    const scheduleUpdate = () => {
        if (animationFrame === null) {
            animationFrame = requestAnimationFrame(update)
        }
    }

    const handlePluginInput = (event: MessageEvent) => {
        if (event.data?.type !== PLUGIN_INPUT_FOCUS_MESSAGE_TYPE) {
            return
        }

        const frame = findPluginFrame(event.source)
        if (!frame) {
            return
        }

        if (event.data.rect === null) {
            if (pluginInput?.frame === frame) {
                pluginInput = null
                scheduleUpdate()
            }
            return
        }

        const bounds = readInputBounds(event.data.rect)
        if (!bounds) {
            return
        }

        pluginInput = { frame, bounds }
        scheduleUpdate()
    }

    node.addEventListener('focusin', scheduleUpdate)
    node.addEventListener('focusout', scheduleUpdate)
    viewport.addEventListener('resize', scheduleUpdate)
    viewport.addEventListener('scroll', scheduleUpdate)
    window.addEventListener('message', handlePluginInput)

    return {
        destroy() {
            node.removeEventListener('focusin', scheduleUpdate)
            node.removeEventListener('focusout', scheduleUpdate)
            viewport.removeEventListener('resize', scheduleUpdate)
            viewport.removeEventListener('scroll', scheduleUpdate)
            window.removeEventListener('message', handlePluginInput)
            if (animationFrame !== null) {
                cancelAnimationFrame(animationFrame)
            }
            restoreShiftedTarget()
        },
    }
}
