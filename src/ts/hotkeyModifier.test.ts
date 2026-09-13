import { expect, it } from 'vitest'
import { shortcutModifier } from './hotkeyModifier'

it('adds Command on Mac while preserving saved Ctrl bindings', () => {
    expect(shortcutModifier({ ctrlKey: false, metaKey: true }, 'macos')).toBe(
        true,
    )
    expect(shortcutModifier({ ctrlKey: true, metaKey: false }, 'macos')).toBe(
        true,
    )
    expect(shortcutModifier({ ctrlKey: false, metaKey: false }, 'macos')).toBe(
        false,
    )
    for (const os of ['windows', 'linux', 'android', 'ios']) {
        expect(shortcutModifier({ ctrlKey: false, metaKey: true }, os)).toBe(
            false,
        )
        expect(shortcutModifier({ ctrlKey: true, metaKey: false }, os)).toBe(
            true,
        )
    }
})
