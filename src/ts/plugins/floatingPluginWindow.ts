// The plugin frame as a floating window: risunestShowFloatingWindow, risunestSetFloatingDragRegion.

export interface FloatingPluginWindowHost {
    iframe: HTMLIFrameElement
    lifetimeSignal: AbortSignal
}

export function createFloatingPluginWindow(host: FloatingPluginWindowHost) {
    void host
    return {
        async show(options?: unknown, onChange?: unknown): Promise<unknown> {
            void options
            void onChange
            throw new Error('risunestShowFloatingWindow is not implemented')
        },
        async setDragRegion(region?: unknown): Promise<void> {
            void region
            throw new Error('risunestSetFloatingDragRegion is not implemented')
        },
        /** Ends floating and removes what it added. showContainer and hideContainer call it. */
        stop(): void {},
    }
}
