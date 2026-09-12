/** Older Android WebViews treat non-special schemes as opaque paths. Parse the
 * authority with the standard HTTPS parser only after checking our own scheme.
 * The result is structural only and must never be fetched as an HTTPS URL. */
export function parseRisuLocalUrl(value: string): URL | null {
  const prefix = /^risunestlocal:\/\//i.exec(value);
  if (!prefix || /[\\\s\u0000-\u001f\u007f]/.test(value)) return null;
  const authority = value.slice(prefix[0].length).split(/[/?#]/, 1)[0];
  // HTTPS would otherwise erase its default port or normalize malformed authority.
  if (authority.includes(":") || authority.includes("@")) return null;
  try {
    return new URL(`https://${value.slice(prefix[0].length)}`);
  } catch {
    return null;
  }
}
