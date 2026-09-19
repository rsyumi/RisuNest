import type { JsonPing, JsonRPC } from './mcplib'

export function writeMCPMessage(
    child: { write(data: string): Promise<void> },
    message: JsonRPC | JsonPing,
): Promise<void> {
    return child.write(`${JSON.stringify(message)}\n`)
}

/** Preserve the configured command and environment; do not invoke a login shell. */
export async function spawnMCPProcess<T>(
    command: string,
    spawn: () => Promise<T>,
): Promise<T> {
    try {
        return await spawn()
    } catch (cause) {
        const detail = cause instanceof Error ? cause.message : String(cause)
        const missing = /ENOENT|os error 2\b|not found|no such file|cannot find the file/i.test(detail)
        const guidance = missing
            ? ' Check that the tool is installed and set env.PATH in this MCP configuration so RisuNest can find it. A command available in Terminal may not be available to a desktop launch.'
            : ''
        throw new Error(`Could not start MCP command ${JSON.stringify(command)}.${guidance} ${detail}`, { cause })
    }
}
