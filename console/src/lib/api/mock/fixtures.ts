import type {
  Backup, ClusterInfo, Deployment, Fragment, Invoice, Member, Metrics,
  RequestLogLine, Role, SchemaResponse, Token, UsageRow, VerifyResult,
} from "../types";

/* Deterministic pseudo-random so every render and every reload agree. */
export function rng(seed: number) {
  let s = seed >>> 0 || 1;
  return () => {
    s ^= s << 13; s >>>= 0;
    s ^= s >> 17;
    s ^= s << 5;  s >>>= 0;
    return s / 4294967296;
  };
}

export const DEPLOYMENTS: Deployment[] = [
  {
    id: "dep_events_prod", name: "events-prod", region: "eu-west-1", host: "big-01.eu-west-1.bigdb.cloud",
    version: "0.1.0", status: { live: true, ready: true }, health: "healthy", created_at: "2026-03-04T09:12:00Z",
    file_bytes: 148_339_982_336, page_count: 36_215_808, qpm: 14_820, p95_ms: 3.1,
    engines: { bitmap: 6, "bitmap+columnar": 3, columnar: 1 }, table_count: 10, cluster: true, role: "admin",
  },
  {
    id: "dep_events_stg", name: "events-staging", region: "eu-west-1", host: "big-07.eu-west-1.bigdb.cloud",
    version: "0.1.0", status: { live: true, ready: true }, health: "healthy", created_at: "2026-04-18T14:41:00Z",
    file_bytes: 9_126_805_504, page_count: 2_228_224, qpm: 412, p95_ms: 1.9,
    engines: { bitmap: 6, "bitmap+columnar": 3, columnar: 1 }, table_count: 10, cluster: false, role: "write",
  },
  {
    id: "dep_ledger", name: "ledger-eu", region: "eu-central-1", host: "big-02.eu-central-1.bigdb.cloud",
    version: "0.1.0", status: { live: true, ready: false }, health: "degraded", created_at: "2026-01-22T08:03:00Z",
    file_bytes: 61_203_251_200, page_count: 14_942_208, qpm: 3_940, p95_ms: 11.4,
    engines: { bitmap: 2, "bitmap+columnar": 4, columnar: 2 }, table_count: 8, cluster: true, role: "admin",
  },
  {
    id: "dep_clicks_us", name: "clickstream-us", region: "us-east-1", host: "big-11.us-east-1.bigdb.cloud",
    version: "0.1.0", status: { live: true, ready: true }, health: "healthy", created_at: "2026-05-30T11:55:00Z",
    file_bytes: 402_653_184_000, page_count: 98_304_000, qpm: 51_210, p95_ms: 4.6,
    engines: { bitmap: 4, "bitmap+columnar": 1, columnar: 0 }, table_count: 5, cluster: true, role: "read",
  },
  {
    id: "dep_analytics_dev", name: "analytics-dev", region: "ap-southeast-1", host: "big-19.ap-southeast-1.bigdb.cloud",
    version: "0.1.0", status: { live: false, ready: false }, health: "failed", created_at: "2026-07-09T16:20:00Z",
    file_bytes: 528_482_304, page_count: 129_024, qpm: 0, p95_ms: 0,
    engines: { bitmap: 3, "bitmap+columnar": 0, columnar: 0 }, table_count: 3, cluster: false, role: "admin",
  },
];

export const SCHEMA: SchemaResponse = {
  tables: [
    {
      name: "events", engine: "bitmap+columnar",
      fields: [
        { name: "country", kind: "set", bit_depth: 0 },
        { name: "device", kind: "mutex", bit_depth: 0 },
        { name: "campaign", kind: "set", bit_depth: 0 },
        { name: "converted", kind: "bool", bit_depth: 0 },
        { name: "amount", kind: "decimal", bit_depth: 32, scale: 2 },
        { name: "duration_ms", kind: "int", bit_depth: 24 },
        { name: "visit", kind: "time_quantum", bit_depth: 0, granularity: ["Y", "M", "D", "H"] },
      ],
    },
    {
      name: "sessions", engine: "bitmap",
      fields: [
        { name: "browser", kind: "mutex", bit_depth: 0 },
        { name: "os", kind: "mutex", bit_depth: 0 },
        { name: "referrer", kind: "set", bit_depth: 0 },
        { name: "bounced", kind: "bool", bit_depth: 0 },
        { name: "pageviews", kind: "int", bit_depth: 16 },
        { name: "started", kind: "time_quantum", bit_depth: 0, granularity: ["D", "H"] },
      ],
    },
    {
      name: "orders", engine: "bitmap+columnar",
      fields: [
        { name: "status", kind: "mutex", bit_depth: 0 },
        { name: "channel", kind: "set", bit_depth: 0 },
        { name: "sku", kind: "set", bit_depth: 0 },
        { name: "total", kind: "decimal", bit_depth: 40, scale: 2 },
        { name: "margin", kind: "signed_int", bit_depth: 32 },
        { name: "items", kind: "int", bit_depth: 12 },
        { name: "placed", kind: "time_quantum", bit_depth: 0, granularity: ["Y", "M", "D"] },
      ],
    },
    {
      name: "impressions", engine: "bitmap",
      fields: [
        { name: "placement", kind: "set", bit_depth: 0 },
        { name: "creative", kind: "set", bit_depth: 0 },
        { name: "viewable", kind: "bool", bit_depth: 0 },
        { name: "bid_micros", kind: "int", bit_depth: 32 },
        { name: "seen", kind: "time_quantum", bit_depth: 0, granularity: ["D", "H"] },
      ],
    },
    {
      name: "invoice_lines", engine: "columnar",
      fields: [
        { name: "account", kind: "set", bit_depth: 0 },
        { name: "line_total", kind: "decimal", bit_depth: 40, scale: 4 },
        { name: "quantity", kind: "int", bit_depth: 16 },
        { name: "adjustment", kind: "signed_int", bit_depth: 24 },
      ],
    },
  ],
};

export const CARDINALITY: Record<string, Record<string, number>> = {
  events: { country: 249, device: 6, campaign: 4_812, converted: 2, amount: 0, duration_ms: 0, visit: 0 },
  sessions: { browser: 14, os: 9, referrer: 92_401, bounced: 2, pageviews: 0, started: 0 },
  orders: { status: 7, channel: 5, sku: 184_902, total: 0, margin: 0, items: 0, placed: 0 },
  impressions: { placement: 1_204, creative: 38_190, viewable: 2, bid_micros: 0, seen: 0 },
  invoice_lines: { account: 21_940, line_total: 0, quantity: 0, adjustment: 0 },
};

export const RECORD_COUNTS: Record<string, number> = {
  events: 4_128_904_112, sessions: 812_004_991, orders: 96_120_884,
  impressions: 21_004_882_190, invoice_lines: 402_119_003,
};

export function fragmentsFor(table: string): Fragment[] {
  const t = SCHEMA.tables.find((x) => x.name === table);
  if (!t) return [];
  const r = rng(table.length * 7919);
  const out: Fragment[] = [];
  for (const f of t.fields) {
    const views = f.kind === "time_quantum" ? (f.granularity ?? ["D"]).map((g) => `standard_${g}`) : ["standard"];
    if (f.kind === "int" || f.kind === "decimal" || f.kind === "signed_int") views.push("bsi");
    for (const view of views) {
      for (let shard = 0; shard < 4; shard++) {
        const array = Math.floor(r() * 900) + 40;
        const bitmap = Math.floor(r() * 320) + 8;
        const run = Math.floor(r() * 140);
        out.push({
          table, field: f.name, view, shard,
          bytes: (array * 1_024 + bitmap * 8_192 + run * 3_100) | 0,
          containers: { array, bitmap, run },
          cardinality: Math.floor(r() * (CARDINALITY[table]?.[f.name] || 1_000)) + 1,
        });
      }
    }
  }
  return out;
}

export const METRICS: Metrics = {
  big_http_requests_total: 8_412_990_113,
  big_http_responses_total: { "2xx": 8_402_118_004, "4xx": 10_802_119, "5xx": 69_990 },
  latency: { p50_ms: 0.41, p95_ms: 3.1, p99_ms: 9.8 },
  big_http_queries_timed_out_total: 1_204,
  big_http_queries_cancelled_total: 8_819,
  big_http_unauthorized_total: 40_218,
  big_http_connections_accepted_total: 91_204_881,
  big_http_connections_rejected_total: 12_004,
  big_page_count: 36_215_808,
  big_free_pages_reusable: 1_882_112,
  big_pages_pending_reclaim_reader: 41_022,
  big_pages_pending_reclaim_retention: 128_904,
  row_key_dictionary_bytes: 3_182_403_584,
  row_key_dictionary_keys: 41_882_004,
};

export function series(seed: number, n: number, base: number, jitter: number, drift = 0) {
  const r = rng(seed);
  const now = Date.now();
  return Array.from({ length: n }, (_, i) => ({
    t: now - (n - 1 - i) * 60_000,
    v: Math.max(0, base + drift * i + (r() - 0.5) * jitter),
  }));
}

export function latencySeries(seed: number, n: number) {
  const r = rng(seed);
  const now = Date.now();
  return Array.from({ length: n }, (_, i) => {
    const p50 = 0.35 + r() * 0.2;
    const p95 = p50 * (5 + r() * 3);
    const p99 = p95 * (2.2 + r() * 1.6);
    return { t: now - (n - 1 - i) * 60_000, p50, p95, p99 };
  });
}

export const CLUSTER: ClusterInfo = {
  schema_leader: "big-01",
  failover_ms: 980,
  nodes: [
    { node: "big-01", host: "big-01.eu-west-1.bigdb.cloud", region: "eu-west-1a", status: { live: true, ready: true }, version: "0.1.0" },
    { node: "big-02", host: "big-02.eu-west-1.bigdb.cloud", region: "eu-west-1b", status: { live: true, ready: true }, version: "0.1.0" },
    { node: "big-03", host: "big-03.eu-west-1.bigdb.cloud", region: "eu-west-1c", status: { live: true, ready: true }, version: "0.1.0" },
    { node: "big-04", host: "big-04.eu-west-1.bigdb.cloud", region: "eu-west-1a", status: { live: true, ready: false }, version: "0.1.0" },
  ],
  ranges: [
    { range: "r0", low: 0, high: 1 << 20, owner: "big-01", replicas: [
      { node: "big-02", lag_records: 0, lag_ms: 0, state: "in_sync" },
      { node: "big-03", lag_records: 0, lag_ms: 0, state: "in_sync" }] },
    { range: "r1", low: 1 << 20, high: 2 << 20, owner: "big-02", replicas: [
      { node: "big-01", lag_records: 0, lag_ms: 0, state: "in_sync" },
      { node: "big-04", lag_records: 18_402, lag_ms: 4_120, state: "behind" }] },
    { range: "r2", low: 2 << 20, high: 3 << 20, owner: "big-03", replicas: [
      { node: "big-01", lag_records: 0, lag_ms: 0, state: "in_sync" },
      { node: "big-02", lag_records: 0, lag_ms: 0, state: "in_sync" }] },
    { range: "r3", low: 3 << 20, high: 4 << 20, owner: "big-01", replicas: [
      { node: "big-03", lag_records: 0, lag_ms: 0, state: "in_sync" },
      { node: "big-04", lag_records: 0, lag_ms: 0, state: "unreachable" }] },
  ],
};

export const VERIFY: VerifyResult = {
  agreed: false,
  checked_at: "2026-08-31T21:04:11Z",
  rows: [
    { range: "r0", field: "events/country", owner_checksum: "9f2c41ab", replicas: [
      { node: "big-02", checksum: "9f2c41ab", agrees: true, behind_records: 0 },
      { node: "big-03", checksum: "9f2c41ab", agrees: true, behind_records: 0 }] },
    { range: "r1", field: "events/campaign", owner_checksum: "3d80ee17", replicas: [
      { node: "big-01", checksum: "3d80ee17", agrees: true, behind_records: 0 },
      { node: "big-04", checksum: "b1704c92", agrees: false, behind_records: 18_402 }] },
    { range: "r1", field: "events/amount", owner_checksum: "77aa0e35", replicas: [
      { node: "big-01", checksum: "77aa0e35", agrees: true, behind_records: 0 },
      { node: "big-04", checksum: "0c99fe4d", agrees: false, behind_records: 18_402 }] },
    { range: "r2", field: "orders/status", owner_checksum: "5501bc6a", replicas: [
      { node: "big-01", checksum: "5501bc6a", agrees: true, behind_records: 0 },
      { node: "big-02", checksum: "5501bc6a", agrees: true, behind_records: 0 }] },
    { range: "r3", field: "orders/sku", owner_checksum: "e4123f08", replicas: [
      { node: "big-03", checksum: "e4123f08", agrees: true, behind_records: 0 },
      { node: "big-04", checksum: "—", agrees: false, behind_records: -1 }] },
  ],
};

export const BACKUPS: Backup[] = [
  { id: "bk_8812", name: "nightly-2026-08-31", node: "big-01", started_at: "2026-08-31T02:00:00Z", duration_ms: 412_004, bytes: 141_902_884_864, source_bytes: 148_339_982_336, format_version: 3, status: "complete" },
  { id: "bk_8811", name: "nightly-2026-08-31", node: "big-02", started_at: "2026-08-31T02:07:12Z", duration_ms: 398_221, bytes: 138_119_004_160, source_bytes: 144_002_112_000, format_version: 3, status: "complete" },
  { id: "bk_8810", name: "pre-migration-v3", node: "big-01", started_at: "2026-08-24T18:41:02Z", duration_ms: 511_887, bytes: 132_004_118_016, source_bytes: 151_882_113_024, format_version: 3, status: "complete" },
  { id: "bk_8809", name: "nightly-2026-08-30", node: "big-03", started_at: "2026-08-30T02:14:31Z", duration_ms: 0, bytes: 0, source_bytes: 143_112_004_096, format_version: 3, status: "failed", error: "no space left on device at page 18,204,113" },
];

/** Relative to now, so "last used" never reads as a negative age on any clock. */
const ago = (ms: number) => new Date(Date.now() - ms).toISOString();

export const TOKENS: Token[] = [
  { id: "tok_01", name: "grafana-scraper", role: "read", deployment_id: "dep_events_prod", created_at: "2026-03-04T09:20:00Z", last_used_at: ago(42_000), prefix: "big_r_7Kq2" },
  { id: "tok_02", name: "ingest-worker-eu", role: "write", deployment_id: "dep_events_prod", created_at: "2026-03-04T09:22:14Z", last_used_at: ago(9_000), prefix: "big_w_Xa41" },
  { id: "tok_03", name: "console-admin", role: "admin", deployment_id: "dep_events_prod", created_at: "2026-03-04T09:25:41Z", last_used_at: ago(3 * 3_600_000), prefix: "big_a_9Pz0" },
  { id: "tok_04", name: "analytics-notebook", role: "read", deployment_id: "dep_events_prod", created_at: "2026-06-11T13:02:00Z", last_used_at: null, prefix: "big_r_Lm83" },
  { id: "tok_05", name: "old-etl", role: "write", deployment_id: "dep_events_prod", created_at: "2026-01-02T10:00:00Z", last_used_at: ago(104 * 86_400_000), prefix: "big_w_Qb17", revoked_at: ago(103 * 86_400_000) },
];

/**
 * Route, status and code have to agree: `/ready` never returns `parse_error`,
 * and an admin token never returns 401. Each entry is one coherent outcome, so
 * the log reads like a log rather than like a shuffle.
 */
const OUTCOMES: Array<{
  method: string; route: string; status: number; code?: string; weight: number;
  roles: Array<Role | "anonymous">; slow?: boolean; sharded?: boolean;
}> = [
  { method: "POST", route: "/table/events/query", status: 200, weight: 30, roles: ["read", "write", "admin"], sharded: true },
  { method: "POST", route: "/table/orders/query", status: 200, weight: 14, roles: ["read", "admin"], sharded: true },
  { method: "POST", route: "/sql", status: 200, weight: 18, roles: ["read", "admin"] },
  { method: "POST", route: "/table/events/import", status: 200, weight: 12, roles: ["write", "admin"] },
  { method: "GET", route: "/schema", status: 200, weight: 8, roles: ["read", "write", "admin"] },
  { method: "GET", route: "/metrics", status: 200, weight: 6, roles: ["read", "admin"] },
  { method: "GET", route: "/ready", status: 200, weight: 6, roles: ["anonymous"] },
  { method: "GET", route: "/health", status: 200, weight: 6, roles: ["anonymous"] },
  { method: "GET", route: "/verify", status: 200, weight: 2, roles: ["read", "admin"], slow: true },

  // refusals: the planner said no, by name, at parse time
  { method: "POST", route: "/sql", status: 400, code: "sql_no_joins", weight: 5, roles: ["read", "admin"] },
  { method: "POST", route: "/sql", status: 400, code: "sql_unsupported", weight: 4, roles: ["read", "admin"] },
  { method: "POST", route: "/sql", status: 400, code: "sql_projection_unsupported", weight: 2, roles: ["read"] },
  { method: "POST", route: "/table/events/query", status: 422, code: "not_pageable", weight: 3, roles: ["read"] },
  { method: "POST", route: "/table/ordrs/query", status: 404, code: "unknown_table", weight: 2, roles: ["read"] },
  { method: "POST", route: "/table/events/query", status: 400, code: "operator_not_allowed", weight: 2, roles: ["read"] },

  // failures: no answer was produced
  { method: "POST", route: "/sql", status: 400, code: "parse_error", weight: 3, roles: ["read", "admin"] },
  { method: "GET", route: "/schema", status: 401, code: "unauthorized", weight: 3, roles: ["anonymous"] },
  { method: "POST", route: "/table/events/import", status: 403, code: "forbidden", weight: 1, roles: ["read"] },
  { method: "POST", route: "/table/events/query", status: 503, code: "owner_unavailable", weight: 2, roles: ["read", "admin"], slow: true, sharded: true },
  { method: "POST", route: "/sql", status: 504, code: "deadline_exceeded", weight: 2, roles: ["read", "admin"], slow: true },
];

const WEIGHTED = OUTCOMES.flatMap((o) => Array<typeof o>(o.weight).fill(o));

export function requestLog(n = 400): RequestLogLine[] {
  const r = rng(4242);
  const now = Date.now();
  return Array.from({ length: n }, (_, i) => {
    const o = WEIGHTED[Math.floor(r() * WEIGHTED.length)];
    return {
      ts: new Date(now - i * 1_337).toISOString(),
      request_id: Array.from({ length: 16 }, () => "0123456789abcdef"[Math.floor(r() * 16)]).join(""),
      method: o.method, route: o.route, status: o.status, code: o.code,
      duration_ms: Number((o.slow ? 40 + r() * 900 : 0.2 + r() * 9).toFixed(2)),
      bytes: Math.floor(r() * 18_000) + 90,
      role: o.roles[Math.floor(r() * o.roles.length)],
      shard: o.sharded && r() > 0.5 ? `s${Math.floor(r() * 4)}` : undefined,
    };
  });
}

export const USAGE: UsageRow[] = [
  { deployment_id: "dep_clicks_us", deployment: "clickstream-us", queries: 2_214_882_004, facts_ingested: 88_204_119_003, storage_pages: 98_304_000, cost_usd: 4_812.44 },
  { deployment_id: "dep_events_prod", deployment: "events-prod", queries: 640_118_229, facts_ingested: 12_004_882_119, storage_pages: 36_215_808, cost_usd: 1_884.02 },
  { deployment_id: "dep_ledger", deployment: "ledger-eu", queries: 170_229_004, facts_ingested: 2_118_004_229, storage_pages: 14_942_208, cost_usd: 742.19 },
  { deployment_id: "dep_events_stg", deployment: "events-staging", queries: 17_882_004, facts_ingested: 402_118_990, storage_pages: 2_228_224, cost_usd: 96.40 },
  { deployment_id: "dep_analytics_dev", deployment: "analytics-dev", queries: 402_118, facts_ingested: 8_119_002, storage_pages: 129_024, cost_usd: 12.08 },
];

export const INVOICES: Invoice[] = [
  { id: "in_2026_08", period: "August 2026", issued_at: "2026-09-01", total_usd: 7_547.13, status: "open" },
  { id: "in_2026_07", period: "July 2026", issued_at: "2026-08-01", total_usd: 7_112.88, status: "paid" },
  { id: "in_2026_06", period: "June 2026", issued_at: "2026-07-01", total_usd: 6_804.02, status: "paid" },
  { id: "in_2026_05", period: "May 2026", issued_at: "2026-06-01", total_usd: 6_119.40, status: "paid" },
];

export const MEMBERS: Member[] = [
  { id: "u_1", name: "Bany", email: "bany@bigdb.cloud", role: "owner", last_active: ago(60_000) },
  { id: "u_2", name: "R. Okonkwo", email: "rok@bigdb.cloud", role: "admin", last_active: ago(4 * 3_600_000) },
  { id: "u_3", name: "L. Marchetti", email: "lm@bigdb.cloud", role: "member", last_active: ago(36 * 3_600_000) },
  { id: "u_4", name: "S. Devi", email: "sd@bigdb.cloud", role: "member", last_active: ago(3 * 86_400_000) },
];
