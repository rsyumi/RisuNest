type RealmAccessEnvironment = {
    VITE_DISABLE_REALM?: string
}

export function isRealmAccessDisabled(
    environment: RealmAccessEnvironment = import.meta.env as RealmAccessEnvironment,
): boolean {
    return environment.VITE_DISABLE_REALM === 'true'
}
