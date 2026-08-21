type RealmAccessEnvironment = {
    VITE_DISABLE_REALM?: string
}

export function isRealmAccessDisabled(
    environment: RealmAccessEnvironment = import.meta.env as RealmAccessEnvironment,
): boolean {
    return environment.VITE_DISABLE_REALM === 'true'
}

export function fetchRealmResource(
    input: RequestInfo | URL,
    init?: RequestInit,
    environment: RealmAccessEnvironment = import.meta.env as RealmAccessEnvironment,
): Promise<Response> | undefined {
    if (isRealmAccessDisabled(environment)) {
        return
    }

    return fetch(input, init)
}
