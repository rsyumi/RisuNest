import { invoke } from '@tauri-apps/api/core'

/** A device section the user can take in or out of synchronization. */
export type LocalDataSection = 'hypa' | 'local-plugins'

export interface LocalDataParticipation {
    section: LocalDataSection
    participating: boolean
}

export async function readLocalDataParticipation(): Promise<LocalDataParticipation[]> {
    return await invoke<LocalDataParticipation[]>('pds_read_section_participation')
}

export async function setLocalDataParticipating(
    section: LocalDataSection,
    participating: boolean,
): Promise<void> {
    await invoke('pds_set_section_participating', { section, participating })
}
