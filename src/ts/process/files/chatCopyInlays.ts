function readBlobAsDataUrl(blob: Blob): Promise<string> {
    return new Promise((resolve, reject) => {
        const reader = new FileReader()
        reader.onload = () => resolve(reader.result as string)
        reader.onerror = () => reject(reader.error ?? new Error('Failed to read copy image'))
        reader.readAsDataURL(blob)
    })
}

export async function copyImageSourceToDataUrl(url: string, quality: number): Promise<string | null> {
    const response = await fetch(url)
    if (!response.ok) return null
    const image = new Image()
    image.crossOrigin = 'anonymous'
    const decoded = new Promise<void>((resolve, reject) => {
        image.onload = () => resolve()
        image.onerror = () => reject(new Error('Failed to decode copy image'))
    })
    image.src = await readBlobAsDataUrl(await response.blob())
    await decoded

    const canvas = document.createElement('canvas')
    const context = canvas.getContext('2d')
    if (!context) throw new Error('Failed to create copy image canvas')
    canvas.width = image.width
    canvas.height = image.height
    context.drawImage(image, 0, 0)
    return canvas.toDataURL('image/jpeg', quality)
}
