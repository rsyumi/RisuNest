import { failureRecord as recordOf } from "./failureRecord.mjs";

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

/**
 * What a failed stage records about `error`: its name, message and stack, and those of each error
 * in its `cause` chain, up to `limit` causes (8 unless given) and stopping at one already listed.
 */
export const failureRecord = recordOf as (error: unknown, limit?: number) => FailureRecord;
