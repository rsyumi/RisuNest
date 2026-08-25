interface HubMessageEvent {
    origin: string
    source: MessageEventSource | null
}

export interface HubPopupController {
    readonly source: Window | null
    open(url: string): Window | null
    close(): void
}

const productionDriveCallbackUrl = 'https://sv.risuai.xyz/drive'

export function resolveExpectedOfficialAccountMessageUrl(
    messageType: unknown,
    hubUrl: string,
    accountIframeUrl: string,
): string {
    return messageType === 'drive'
        ? productionDriveCallbackUrl
        : (accountIframeUrl || `${hubUrl}/hub/login`)
}

export function createHubPopupController(
    openWindow: (url: string) => Window | null = (url) => window.open(url),
): HubPopupController {
    let popup: Window | null = null
    return {
        get source() {
            if (popup?.closed) popup = null
            return popup
        },
        open(url) {
            if (this.source) return popup
            popup = openWindow(url)
            return popup
        },
        close() {
            const current = popup
            popup = null
            if (current && !current.closed) current.close()
        },
    }
}

export function isExpectedHubMessage(
    event: HubMessageEvent,
    expectedUrl: string,
    expectedSource?: MessageEventSource | null,
): boolean {
    let expectedOrigin: string
    try {
        expectedOrigin = new URL(expectedUrl, globalThis.location?.href).origin
    } catch (error) {
        return false
    }
    if (event.origin !== expectedOrigin) return false
    if (expectedSource == null || event.source == null) return false
    if ((expectedSource as { closed?: boolean }).closed === true) return false
    return event.source === expectedSource
}
