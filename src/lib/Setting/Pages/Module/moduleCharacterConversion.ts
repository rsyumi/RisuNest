import type { character, groupChat } from 'src/ts/storage/database.svelte'

type CompleteCharacter = character | groupChat

interface ModuleCharacterConversionDependencies {
    commit(character: CompleteCharacter, reason: string): Promise<unknown>
    onSuccess(): void
    onError(error: unknown): void
}

export async function commitModuleCharacterConversion(
    character: CompleteCharacter,
    dependencies: ModuleCharacterConversionDependencies,
): Promise<void> {
    try {
        await dependencies.commit(character, 'convert-module-to-character')
    } catch (error) {
        dependencies.onError(error)
        return
    }
    dependencies.onSuccess()
}
