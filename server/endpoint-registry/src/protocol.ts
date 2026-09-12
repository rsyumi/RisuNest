// The registry validates the envelope's wire shape, never its encrypted contents.
export const MAX_URL_BYTES = 4096;
export const MAX_ENVELOPE_BYTES = 12 + MAX_URL_BYTES + 16;
export const MAX_BODY_BYTES = Math.ceil((MAX_ENVELOPE_BYTES * 8) / 6);
const UUID_V4 =
  /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export class ProtocolError extends Error {
  constructor(
    public readonly code: string,
    public readonly status = 400,
  ) {
    super(code);
  }
}

export function endpointId(url: URL): string {
  const match = /^\/endpoints\/([^/]+)$/.exec(url.pathname);
  if (!match) throw new ProtocolError("not-found", 404);
  if (url.search) throw new ProtocolError("query-not-allowed");
  const uuid = match[1]!;
  if (!UUID_V4.test(uuid)) throw new ProtocolError("invalid-uuid");
  return uuid.toLowerCase();
}

export function validateEnvelope(envelope: string): string {
  if (envelope.length > MAX_BODY_BYTES)
    throw new ProtocolError("body-too-large", 413);
  if (!/^[A-Za-z0-9_-]+$/.test(envelope) || envelope.length % 4 === 1)
    throw new ProtocolError("invalid-envelope");
  let decoded: string;
  try {
    decoded = atob(envelope.replaceAll("-", "+").replaceAll("_", "/"));
  } catch {
    throw new ProtocolError("invalid-envelope");
  }
  if (decoded.length < 29 || decoded.length > MAX_ENVELOPE_BYTES)
    throw new ProtocolError("invalid-envelope");
  const canonical = btoa(decoded)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
  if (canonical !== envelope) throw new ProtocolError("invalid-envelope");
  return envelope;
}

export async function readEnvelope(request: Request): Promise<string> {
  const type = request.headers.get("content-type")?.trim() ?? "";
  if (!/^text\/plain(?:\s*;\s*charset=utf-8)?$/i.test(type))
    throw new ProtocolError("unsupported-media-type", 415);
  const encoding = request.headers.get("content-encoding");
  if (encoding && encoding.toLowerCase() !== "identity")
    throw new ProtocolError("unsupported-content-encoding", 415);
  const length = request.headers.get("content-length");
  if (length !== null) {
    if (!/^[0-9]+$/.test(length))
      throw new ProtocolError("invalid-content-length");
    if (Number(length) > MAX_BODY_BYTES)
      throw new ProtocolError("body-too-large", 413);
  }
  if (!request.body) throw new ProtocolError("invalid-envelope");
  const reader = request.body.getReader();
  const bytes = new Uint8Array(MAX_BODY_BYTES);
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (size + value.byteLength > MAX_BODY_BYTES) {
        await reader.cancel().catch(() => {});
        throw new ProtocolError("body-too-large", 413);
      }
      bytes.set(value, size);
      size += value.byteLength;
    }
    return validateEnvelope(
      new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(
        bytes.subarray(0, size),
      ),
    );
  } catch (error) {
    if (error instanceof ProtocolError) throw error;
    throw new ProtocolError("invalid-body");
  } finally {
    reader.releaseLock();
  }
}
