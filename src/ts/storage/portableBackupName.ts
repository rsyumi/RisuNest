import { localFileStamp } from '../localFileStamp'

export function portableBackupSuggestedName(now = new Date()): string {
    return `risunest-${localFileStamp(now.getTime())}.risunest`
}
