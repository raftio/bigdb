/**
 * Types mirror the `bigd` HTTP surface exactly. Nothing here describes a
 * capability the server does not have — if it is not a route, it is not a type.
 */

/* ── schema ─────────────────────────────────────────────────────────────── */

/** Chosen at CREATE, never changed. Shown wherever a table is named. */
export type TableEngine = "bitmap" | "bitmap+columnar" | "columnar";

/** Which convention decides the row a value goes into. */
export type FieldKind = "set" | "mutex" | "bool" | "int" | "signed_int" | "decimal" | "time_quantum";

export type Granularity = "Y" | "M" | "D" | "H";

export interface FieldInfo {
  name: string;
  kind: FieldKind;
  /** Bits per value for the integer kinds; 0 for the rest. */
  bit_depth: number;
  /** Digits after the point for a decimal; absent otherwise. */
  scale?: number;
  /** Views a time-quantum field writes; absent otherwise. */
  granularity?: Granularity[];
}

export interface TableInfo {
  name: string;
  engine: TableEngine;
  fields: FieldInfo[];
}

/** GET /schema */
export interface SchemaResponse {
  tables: TableInfo[];
}

/* ── errors ─────────────────────────────────────────────────────────────── */

/**
 * Every failure on the wire: `{"code","message"}` plus the status.
 * A refusal is one of these — same shape, different meaning, its own UI.
 */
export interface ApiErrorBody {
  code: string;
  message: string;
  /** 503 only: the shards this owner holds, named so the operator knows the blast radius. */
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

/* ── query surfaces ─────────────────────────────────────────────────────── */

/** POST /sql → {"columns":[...],"rows":[[...]]} */
export interface SqlResult {
  columns: string[];
  rows: Array<Array<string | number | null>>;
}

/**
 * A PQL answer has a shape, and the shape decides how it renders. The server
 * returns the bare JSON; the discriminant is reconstructed from the call.
 */
export type PqlResult =
  | { shape: "count"; value: number }
  | { shape: "aggregate"; call: "Sum" | "Min" | "Max"; field: string; value: number; scale?: number }
  | { shape: "topn"; field: string; items: Array<{ key: string; count: number }> }
  | { shape: "groups"; by: string[]; aggregate?: string; groups: Array<{ key: string[]; value: number }> }
  | { shape: "distinct"; field: string; values: string[] }
  | { shape: "records"; ids: number[]; after?: number | null; limit: number };

export type QueryResult =
  | { kind: "sql"; result: SqlResult }
  | { kind: "pql"; result: PqlResult };

/** Where the time went: parse → plan → fan-out per shard → merge. */
export interface QueryTiming {
  parse_us: number;
  plan_us: number;
  merge_us: number;
  shards: Array<{ shard: string; node: string; us: number; rows_touched: number }>;
  total_us: number;
}

/* ── health & readiness ─────────────────────────────────────────────────── */

export type Health = "healthy" | "degraded" | "failed" | "unknown";

export interface NodeStatus {
  /** GET /health — the process is alive. */
  live: boolean;
  /** GET /ready — it will answer. */
  ready: boolean;
}

/* ── metrics (Prometheus, parsed) ───────────────────────────────────────── */

export interface Metrics {
  big_http_requests_total: number;
  big_http_responses_total: Record<"2xx" | "4xx" | "5xx", number>;
  /** Derived from the duration histogram — never a mean. */
  latency: { p50_ms: number; p95_ms: number; p99_ms: number };
  big_http_queries_timed_out_total: number;
  big_http_queries_cancelled_total: number;
  big_http_unauthorized_total: number;
  big_http_connections_accepted_total: number;
  big_http_connections_rejected_total: number;
  big_page_count: number;
  big_free_pages_reusable: number;
  big_pages_pending_reclaim_reader: number;
  big_pages_pending_reclaim_retention: number;
  /** The one allocation that grows with cardinality. */
  row_key_dictionary_bytes: number;
  row_key_dictionary_keys: number;
}

export interface Series {
  t: number;
  v: number;
}

export interface LatencySeries {
  t: number;
  p50: number;
  p95: number;
  p99: number;
}

/* ── deployments ────────────────────────────────────────────────────────── */

/** One `bigd` process over one file. One tenant. Its own token set. */
export interface Deployment {
  id: string;
  name: string;
  region: string;
  host: string;
  version: string;
  status: NodeStatus;
  health: Health;
  created_at: string;
  file_bytes: number;
  page_count: number;
  qpm: number;
  p95_ms: number;
  engines: Record<TableEngine, number>;
  table_count: number;
  cluster: boolean;
  /** Role of the token this console is holding for this deployment. */
  role: Role;
}

/* ── access ─────────────────────────────────────────────────────────────── */

/** Roles are verbs, not rows. */
export type Role = "read" | "write" | "admin";

export interface Token {
  id: string;
  name: string;
  role: Role;
  deployment_id: string;
  created_at: string;
  last_used_at: string | null;
  prefix: string;
  /** Present exactly once, at creation. */
  secret?: string;
  revoked_at?: string | null;
}

/* ── cluster ────────────────────────────────────────────────────────────── */

export interface RangeInfo {
  range: string;
  low: number;
  high: number;
  owner: string;
  replicas: Array<{ node: string; lag_records: number; lag_ms: number; state: "in_sync" | "behind" | "unreachable" }>;
}

export interface ClusterInfo {
  /** One schema leader owns every row key. */
  schema_leader: string;
  failover_ms: number;
  nodes: Array<{ node: string; host: string; region: string; status: NodeStatus; version: string }>;
  ranges: RangeInfo[];
}

/** GET /verify — do the copies of every range agree. */
export interface VerifyResult {
  agreed: boolean;
  checked_at: string;
  rows: Array<{
    range: string;
    field: string;
    owner_checksum: string;
    replicas: Array<{ node: string; checksum: string; agrees: boolean; behind_records: number }>;
  }>;
}

/* ── storage ────────────────────────────────────────────────────────────── */

/** A fragment is addressed by (table, field, view, shard). */
export interface Fragment {
  table: string;
  field: string;
  view: string;
  shard: number;
  bytes: number;
  containers: { array: number; bitmap: number; run: number };
  cardinality: number;
}

/* ── backups ────────────────────────────────────────────────────────────── */

/** Backup, compaction and format migration are the same operation. */
export interface Backup {
  id: string;
  name: string;
  node: string;
  started_at: string;
  duration_ms: number;
  bytes: number;
  source_bytes: number;
  format_version: number;
  status: "complete" | "running" | "failed";
  error?: string;
}

/* ── ingest ─────────────────────────────────────────────────────────────── */

export interface IngestChunkAck {
  /** Byte offset written. A resend from here is idempotent. */
  offset: number;
  accepted: number;
  rejected: Array<{ line: number; text: string; reason: string; code: string }>;
}

export interface IngestJob {
  id: string;
  table: string;
  filename: string;
  total_bytes: number;
  acked_offset: number;
  accepted: number;
  rejected: number;
  status: "validating" | "uploading" | "paused" | "complete" | "failed";
  throughput: Series[];
  errors: Array<{ line: number; text: string; reason: string; code: string }>;
}

/* ── request log ────────────────────────────────────────────────────────── */

export interface RequestLogLine {
  ts: string;
  request_id: string;
  method: string;
  route: string;
  status: number;
  code?: string;
  duration_ms: number;
  bytes: number;
  role: Role | "anonymous";
  shard?: string;
}

/* ── billing ────────────────────────────────────────────────────────────── */

export interface UsageRow {
  deployment_id: string;
  deployment: string;
  queries: number;
  facts_ingested: number;
  storage_pages: number;
  cost_usd: number;
}

export interface Invoice {
  id: string;
  period: string;
  issued_at: string;
  total_usd: number;
  status: "paid" | "open" | "past_due";
}

export interface Member {
  id: string;
  name: string;
  email: string;
  role: "owner" | "admin" | "member";
  last_active: string;
}
