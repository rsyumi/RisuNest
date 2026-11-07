/**
 * What a failed stage records about `error`: its name, message and stack, and those of each error
 * in its `cause` chain, up to `limit` causes and stopping at one already listed. It refers to nothing
 * outside itself, so a CDP driver can run its source in the page that threw.
 */
export function failureRecord(error, limit = 8) {
  const text = (value) => {
    if (typeof value === "string") return value;
    try {
      return JSON.stringify(value) ?? String(value);
    } catch {
      return String(value);
    }
  };
  const entry = (value) => {
    if (typeof value === "object" && value !== null) {
      const { name, message, stack } = value;
      if (typeof message === "string") {
        return {
          ...(typeof name === "string" ? { name } : {}),
          message,
          ...(typeof stack === "string" && stack ? { stack } : {}),
        };
      }
    }
    return { message: text(value) };
  };
  const causeOf = (value) => (typeof value === "object" && value !== null && "cause" in value ? value.cause : undefined);
  const record = entry(error);
  const causes = [];
  const seen = new Set([error]);
  for (let cause = causeOf(error); cause !== undefined; cause = causeOf(cause)) {
    if (seen.has(cause)) {
      record.causesCut = "cycle";
      break;
    }
    if (causes.length === limit) {
      record.causesCut = "limit";
      break;
    }
    seen.add(cause);
    causes.push(entry(cause));
  }
  if (causes.length > 0) record.causes = causes;
  return record;
}
