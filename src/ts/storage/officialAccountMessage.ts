interface HubMessageEvent {
    origin: string
    source: MessageEventSource | null
}

export function resolveExpectedOfficialAccountMessageUrl(
    _messageType: unknown,
    hubUrl: string,
    accountIframeUrl: string,
): string {
    return accountIframeUrl || `${hubUrl}/hub/login`
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
