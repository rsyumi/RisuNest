/**
 * The route from a failure to the data check. A failure reports what went wrong and offers this;
 * nothing on the ordinary start path opens it on its own.
 */
export const DATA_HEALTH_SECTION_ID = 'risunest-data-health'

export function createDataHealthNavigationQueue() {
    let pending = false
    let receiver: (() => void) | undefined
    return {
        /** Asks for the data check. Held until a screen is listening, so a failure during
         *  startup still reaches it. */
        request(): void {
            if (receiver) receiver()
            else pending = true
        },
        subscribe(listener: () => void): () => void {
            receiver = listener
            if (pending) {
                pending = false
                listener()
            }
            return () => {
                if (receiver === listener) receiver = undefined
            }
        },
    }
}

export const dataHealthNavigation = createDataHealthNavigationQueue()

export function openDataHealthScreen(): void {
    dataHealthNavigation.request()
}
