import type { Database } from 'src/ts/storage/database.svelte'

export const navigationDBState = $state({
    db: {
        characters: [],
        characterOrder: [],
        menuSideBar: true,
        roundIcons: false,
    } as Database,
})
