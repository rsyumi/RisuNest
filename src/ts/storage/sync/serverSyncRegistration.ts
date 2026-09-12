import type { ServerConfig, ServerDirectory } from "./serverSync";

export const REGISTRATION_PREFIX = "risunestlocal://sync-server/register#";
export const MAX_REGISTRATION_URI_BYTES = 2048;
export class RegistrationError extends Error {
  constructor(readonly code: string) {
    super(code);
    this.name = "RegistrationError";
  }
}
function invalid(code = "invalid-registration"): never {
  throw new RegistrationError(code);
}
function base64url(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}
function decode(value: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]+$/.test(value)) invalid("invalid-base64url");
  let bytes: Uint8Array;
  try {
    bytes = Uint8Array.from(
      atob(value.replaceAll("-", "+").replaceAll("_", "/")),
      (c) => c.charCodeAt(0),
    );
  } catch {
    return invalid("invalid-base64url");
  }
  if (base64url(bytes) !== value) invalid("invalid-base64url");
  return bytes;
}
// This schema contains only objects and strings. Parse that small grammar so duplicate
// fields (including escaped names) cannot disappear through JSON.parse's last-write rule.
function strictObject(text: string): Record<string, unknown> {
  let offset = 0;
  function space() {
    while (/[ \t\r\n]/.test(text[offset] ?? "~")) offset++;
  }
  function string(): string {
    space();
    const start = offset;
    if (text[offset++] !== '"') return invalid();
    while (offset < text.length) {
      const char = text[offset++];
      if (char === "\\") {
        offset++;
        continue;
      }
      if (char === '"') {
        let value: string;
        try {
          value = JSON.parse(text.slice(start, offset));
        } catch {
          return invalid();
        }
        for (let i = 0; i < value.length; i++) {
          const code = value.charCodeAt(i);
          if (code >= 0xd800 && code <= 0xdbff) {
            const next = value.charCodeAt(++i);
            if (!(next >= 0xdc00 && next <= 0xdfff)) invalid();
          } else if (code >= 0xdc00 && code <= 0xdfff) invalid();
        }
        return value;
      }
    }
    return invalid();
  }
  function object(depth: number): Record<string, unknown> {
    space();
    if (depth > 2 || text[offset++] !== "{") return invalid();
    const result: Record<string, unknown> = Object.create(null);
    space();
    if (text[offset] === "}") {
      offset++;
      return result;
    }
    while (offset < text.length) {
      const key = string();
      if (Object.hasOwn(result, key)) invalid();
      space();
      if (text[offset++] !== ":") invalid();
      space();
      result[key] = text[offset] === "{" ? object(depth + 1) : string();
      space();
      const separator = text[offset++];
      if (separator === "}") return result;
      if (separator !== ",") invalid();
    }
    return invalid();
  }
  const value = object(1);
  space();
  if (offset !== text.length) invalid();
  return value;
}
function fields(
  value: Record<string, unknown>,
  required: string[],
  optional: string[] = [],
) {
  if (
    required.some((key) => !Object.hasOwn(value, key)) ||
    Object.keys(value).some(
      (key) => !required.includes(key) && !optional.includes(key),
    )
  )
    invalid();
}
function text(value: unknown): string {
  if (typeof value !== "string") return invalid();
  return value;
}
export function validateRegistrationEndpoint(value: string): void {
  if (
    !/^https?:\/\//i.test(value) ||
    new TextEncoder().encode(value).length > 4096 ||
    /[\s\u0000-\u001f\u007f-\u009f\\]/u.test(value)
  )
    invalid("invalid-endpoint");
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return invalid("invalid-endpoint");
  }
  const authority = value.slice(value.indexOf(":") + 3).split(/[/?#]/, 1)[0];
  if (
    !url.hostname ||
    url.username ||
    url.password ||
    authority.includes("@") ||
    value.includes("?") ||
    value.includes("#")
  )
    invalid("invalid-endpoint");
  const loopback =
    url.hostname === "localhost" ||
    url.hostname === "[::1]" ||
    /^127\.\d+\.\d+\.\d+$/.test(url.hostname);
  if (url.protocol !== "https:" && !(url.protocol === "http:" && loopback))
    invalid("https-required");
}
export function validateRegistration(config: ServerConfig): void {
  validateRegistrationEndpoint(config.endpoint);
  if (
    ![config.libraryId, config.deviceId].every((id) =>
      /^[A-Za-z0-9_-]{1,128}$/.test(id),
    )
  )
    invalid("invalid-id");
  if (!/^[a-fA-F0-9]{64}$/.test(config.token)) invalid("invalid-device-token");
  if (config.directory) {
    validateRegistrationEndpoint(config.directory.baseUrl);
    if (
      !/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i.test(
        config.directory.uuid,
      )
    )
      invalid("invalid-directory-uuid");
    if (
      config.directory.key.length !== 43 ||
      decode(config.directory.key).length !== 32
    )
      invalid("invalid-directory-key");
  }
}
export function parseServerRegistration(uri: string): ServerConfig {
  if (uri.length > MAX_REGISTRATION_URI_BYTES)
    invalid("registration-too-large");
  if (!uri.startsWith(REGISTRATION_PREFIX)) invalid("invalid-registration-uri");
  let json: string;
  try {
    json = new TextDecoder("utf-8", { fatal: true }).decode(
      decode(uri.slice(REGISTRATION_PREFIX.length)),
    );
  } catch (error) {
    if (error instanceof RegistrationError) throw error;
    return invalid();
  }
  const value = strictObject(json);
  fields(value, ["endpoint", "libraryId", "deviceId", "token"], ["directory"]);
  let directory: ServerDirectory | undefined;
  if (Object.hasOwn(value, "directory")) {
    if (typeof value.directory !== "object" || value.directory === null)
      invalid();
    const d = value.directory as Record<string, unknown>;
    fields(d, ["baseUrl", "uuid", "key"]);
    directory = {
      baseUrl: text(d.baseUrl),
      uuid: text(d.uuid).toLowerCase(),
      key: text(d.key),
    };
  }
  const config: ServerConfig = {
    endpoint: text(value.endpoint),
    libraryId: text(value.libraryId),
    deviceId: text(value.deviceId),
    token: text(value.token),
    ...(directory ? { directory } : {}),
  };
  validateRegistration(config);
  return config;
}
export function encodeServerRegistration(config: ServerConfig): string {
  validateRegistration(config);
  const uri =
    REGISTRATION_PREFIX +
    base64url(
      new TextEncoder().encode(
        JSON.stringify({
          endpoint: config.endpoint,
          libraryId: config.libraryId,
          deviceId: config.deviceId,
          token: config.token,
          ...(config.directory ? { directory: config.directory } : {}),
        }),
      ),
    );
  if (uri.length > MAX_REGISTRATION_URI_BYTES)
    invalid("registration-too-large");
  return uri;
}
