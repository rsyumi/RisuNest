/** `YYYY-MM-DD-HH-mm-ss` in the device's time zone, for the names of files the user saves. */
export function localFileStamp(time: number): string {
    const date = new Date(time)
    const pad = (value: number) => String(value).padStart(2, '0')
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`
        + `-${pad(date.getHours())}-${pad(date.getMinutes())}-${pad(date.getSeconds())}`
}
