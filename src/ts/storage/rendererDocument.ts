declare global {
    var __risunestRendererDocument: string | undefined
}

/**
 * Names this page document to the native store. Opening the store under a new
 * name ends the reads, CAS jobs and media transfers that earlier documents
 * left open, and the store refuses new ones from those documents.
 */
export function rendererDocument(): string {
    // Kept on the global so a module that hot reload evaluates again keeps the name.
    return globalThis.__risunestRendererDocument ??= crypto.randomUUID()
}
