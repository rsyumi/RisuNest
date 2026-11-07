/** One error in a failure record. */
export interface FailureEntry {
  name?: string;
  message: string;
  stack?: string;
}

export interface FailureRecord extends FailureEntry {
  /** The errors of the `cause` chain, nearest first. */
  causes?: FailureEntry[];
  /** Set when the chain goes on past the last listed cause: it is longer than the limit, or a cause repeats. */
  causesCut?: "limit" | "cycle";
}

const MAX_CAUSES = 8;

function text(value: unknown) {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

function entry(value: unknown): FailureEntry {
  if (typeof value === "object" && value !== null) {
    const { name, message, stack } = value as { name?: unknown; message?: unknown; stack?: unknown };
    if (typeof message === "string") {
      return {
        ...(typeof name === "string" ? { name } : {}),
        message,
        ...(typeof stack === "string" && stack ? { stack } : {}),
      };
    }
  }
  return { message: text(value) };
}

function causeOf(value: unknown) {
  return typeof value === "object" && value !== null && "cause" in value
    ? (value as { cause?: unknown }).cause
    : undefined;
}

/**
 * What a failed stage records about `error`: its name, message and stack, and those of each error
 * in its `cause` chain, up to `limit` causes and stopping at one already listed.
 */
export function failureRecord(error: unknown, limit = MAX_CAUSES): FailureRecord {
  const record: FailureRecord = entry(error);
  const causes: FailureEntry[] = [];
  const seen = new Set<unknown>([error]);
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
