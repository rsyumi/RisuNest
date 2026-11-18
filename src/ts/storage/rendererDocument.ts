declare global {
    var __risunestRendererDocument: string | undefined
}

/**
 * Names this page document to the native store. Opening the store under a new
 * name ends the reads, CAS jobs and media transfers that earlier documents
 * left open, and the store refuses new ones from those documents. The name
 * starts with its creation time, which tells the store the later page when
 * the first opens of two pages arrive out of order.
 */
export function rendererDocument(): string {
    // Kept on the global so a module that hot reload evaluates again keeps the name.
    return globalThis.__risunestRendererDocument ??= `${Date.now()}-${crypto.randomUUID()}`
}
