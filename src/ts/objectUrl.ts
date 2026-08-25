export async function withObjectUrl<T>(
    blob: Blob,
    consume: (url: string) => T | PromiseLike<T>,
): Promise<T> {
    const url = URL.createObjectURL(blob)
    try {
        return await consume(url)
    }
    finally {
        URL.revokeObjectURL(url)
    }
}

export function downloadBlobWithObjectUrl(blob: Blob, filename: string): void {
    const url = URL.createObjectURL(blob)
    try {
        const anchor = document.createElement('a')
        anchor.href = url
        anchor.download = filename
        anchor.click()
    }
    finally {
        setTimeout(() => URL.revokeObjectURL(url), 0)
    }
}
