import { keiServerURL } from "./kei"
import { getDatabase } from "../storage/database.svelte"

let lastKeiSave = 0

export function saveDbKei() {
    try {
        const db = getDatabase()
        if (!db?.account?.kei) {
            return
        }
        if (Date.now() - lastKeiSave < 60000 * 5) {
            return
        }
        lastKeiSave = Date.now()
        fetch(keiServerURL() + '/autobackup/save', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({
                token: db.account.token,
                database: db
            })
        }).catch((error) => {
            console.error('Kei auto backup failed:', error)
        })
    } catch (error) {}
}
