/**
 * The slice of the wire contract this page actually renders.
 *
 * The console carries the full set; the landing only needs enough to show a
 * real refusal and to name a table's engine. Kept structurally identical to
 * `console/src/lib/api/types.ts` so a component can move between the two
 * without edits.
 */

/** Chosen at CREATE, never changed. Shown wherever a table is named. */
export type TableEngine = "bitmap" | "bitmap+columnar" | "columnar";

/** Every failure on the wire: `{"code","message"}` plus the status. */
export interface ApiErrorBody {
  code: string;
  message: string;
  /** 503 only: the shards this owner holds, named so the blast radius is clear. */
  shards?: string[];
}

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly shards?: string[];
  /** Byte offset into the submitted query, when the server located the fault. */
  readonly at?: number;
  readonly span?: [number, number];

  constructor(status: number, body: ApiErrorBody, span?: [number, number], at?: number) {
    super(body.message);
    this.name = "ApiError";
    this.status = status;
    this.code = body.code;
    this.shards = body.shards;
    this.span = span;
    this.at = at;
  }
}
