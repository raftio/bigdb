import { ApiError, type Backup, type ClusterInfo, type Deployment, type Fragment, type Invoice,
  type Member, type Metrics, type RequestLogLine, type Role, type SchemaResponse, type TableEngine,
  type Token, type UsageRow, type VerifyResult, type FieldKind, type FieldInfo, type TableInfo,
  type IngestChunkAck } from "./types";
import * as fx from "./mock/fixtures";
import { runPql, runSql, type Executed } from "./mock/planner";

/**
 * One typed client, one method per route. Nothing here reaches a capability the
 * server does not have. `MOCK` swaps the transport, not the shape: every method
 * returns exactly what `bigd` returns.
 */

const MOCK = process.env.NEXT_PUBLIC_BIGDB_API === undefined;

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Latency the console must be honest about, so loading states get exercised. */
const lag = () => wait(120 + Math.random() * 240);

/** Roles are verbs. The client refuses locally what the server would refuse anyway. */
export function requires(role: Role, need: Role): boolean {
  const rank: Record<Role, number> = { read: 0, write: 1, admin: 2 };
  return rank[role] >= rank[need];
}

export function denialReason(role: Role, need: Role): string {
  return `Your token on this deployment is \`${role}\`. This action needs \`${need}\`.`;
}

function unauthorized(need: Role): never {
  throw new ApiError(403, { code: "forbidden", message: `this route requires the \`${need}\` role` });
}

/* ── deployment-scoped client ───────────────────────────────────────────── */

export class BigClient {
  constructor(
    readonly deploymentId: string,
    readonly role: Role,
    private readonly baseUrl?: string,
  ) {}

  private guard(need: Role) {
    if (!requires(this.role, need)) unauthorized(need);
  }

  /** GET /health — unauthenticated. */
  async health(): Promise<boolean> {
    await lag();
    return fx.DEPLOYMENTS.find((d) => d.id === this.deploymentId)?.status.live ?? false;
  }

  /** GET /ready — unauthenticated. */
  async ready(): Promise<boolean> {
    await lag();
    const d = fx.DEPLOYMENTS.find((x) => x.id === this.deploymentId);
    if (!d) return false;
    if (!d.status.live) throw new ApiError(503, { code: "unreachable", message: `no response from ${d.host}` });
    return d.status.ready;
  }

  /** GET /metrics — read. */
  async metrics(): Promise<Metrics> {
    this.guard("read");
    await lag();
    const d = fx.DEPLOYMENTS.find((x) => x.id === this.deploymentId);
    if (d && !d.status.live) throw new ApiError(503, { code: "unreachable", message: `no response from ${d.host}` });
    return { ...fx.METRICS, big_page_count: d?.page_count ?? fx.METRICS.big_page_count };
  }

  /** GET /schema — read. */
  async schema(): Promise<SchemaResponse> {
    this.guard("read");
    await lag();
    const d = fx.DEPLOYMENTS.find((x) => x.id === this.deploymentId);
    if (d && !d.status.live) throw new ApiError(503, { code: "unreachable", message: `no response from ${d.host}` });
    return fx.SCHEMA;
  }

  /** GET /verify — read. Do the copies of every range agree. */
  async verify(): Promise<VerifyResult> {
    this.guard("read");
    await wait(900);
    return fx.VERIFY;
  }

  /** POST /repair — admin. Catch up every copy that is behind. */
  async repair(onProgress?: (pct: number, note: string) => void): Promise<void> {
    this.guard("admin");
    const steps: Array<[number, string]> = [
      [8, "reading range map from the schema leader"],
      [24, "r1 → big-04: streaming events/campaign"],
      [51, "r1 → big-04: streaming events/amount"],
      [70, "r3 → big-04: node unreachable, retrying"],
      [88, "r3 → big-04: streaming orders/sku"],
      [100, "every copy agrees"],
    ];
    for (const [pct, note] of steps) { await wait(650); onProgress?.(pct, note); }
  }

  /** POST /table/{t}/query?after=&limit= — read. Body is one PQL call. */
  async query(table: string, pql: string, opts: { after?: string; limit?: number; signal?: AbortSignal } = {}): Promise<Executed> {
    this.guard("read");
    await this.race(opts.signal);
    return runPql(pql, table, opts.limit ?? 0, opts.after);
  }

  /** POST /sql — read; admin for CREATE TABLE. */
  async sql(statement: string, opts: { signal?: AbortSignal } = {}): Promise<Executed> {
    if (/^\s*CREATE\s+TABLE\b/i.test(statement)) this.guard("admin");
    else this.guard("read");
    await this.race(opts.signal);
    return runSql(statement);
  }

  private async race(signal?: AbortSignal) {
    const ms = 200 + Math.random() * 500;
    await new Promise<void>((resolve, reject) => {
      const t = setTimeout(resolve, ms);
      signal?.addEventListener("abort", () => {
        clearTimeout(t);
        reject(new ApiError(499, { code: "query_cancelled", message: "the client closed the request before the deadline" }));
      });
    });
  }

  /** POST /table/{t}/import — write. One fact per line. */
  async import(table: string, chunk: string, offset: number): Promise<IngestChunkAck> {
    this.guard("write");
    await wait(220);
    const lines = chunk.split("\n").filter(Boolean);
    const fields = fieldIndex(fx.SCHEMA.tables.find((t) => t.name === table));
    const rejected = lines.flatMap((text, i) => {
      const bad = validateFactLine(text, fields);
      return bad ? [{ line: i + 1, text, reason: bad.reason, code: bad.code }] : [];
    });
    return { offset: offset + chunk.length, accepted: lines.length - rejected.length, rejected };
  }

  /** POST /table/{t}/delete — write. One record id per line. */
  async deleteRecords(table: string, ids: string): Promise<{ deleted: number }> {
    this.guard("write");
    await lag();
    return { deleted: ids.split("\n").filter(Boolean).length };
  }

  /** POST /table/{t}?engine= — admin. */
  async createTable(table: string, engine: TableEngine): Promise<void> {
    this.guard("admin"); await lag();
  }

  /** POST /table/{t}/field/{f}?kind=&bit_depth=&scale= — admin. */
  async createField(table: string, field: string, p: { kind: FieldKind; bit_depth?: number; scale?: number }): Promise<void> {
    this.guard("admin"); await lag();
  }

  /** DELETE /table/{t} — admin. */
  async dropTable(table: string): Promise<void> { this.guard("admin"); await lag(); }

  /** DELETE /table/{t}/field/{f} — admin. */
  async dropField(table: string, field: string): Promise<void> { this.guard("admin"); await lag(); }

  /** POST /admin/backup?name= — admin. One node's file, not a cluster snapshot. */
  async backup(name: string, node: string): Promise<Backup> {
    this.guard("admin");
    await wait(1_400);
    return {
      id: `bk_${Math.floor(Math.random() * 9000) + 1000}`, name, node,
      started_at: new Date().toISOString(), duration_ms: 388_112,
      bytes: 139_004_882_944, source_bytes: 148_339_982_336, format_version: 3, status: "complete",
    };
  }

  /* — control-plane data, not a `bigd` route: the SaaS layer's own store — */

  async fragments(table: string): Promise<Fragment[]> { await lag(); return fx.fragmentsFor(table); }
  async cluster(): Promise<ClusterInfo> { this.guard("read"); await lag(); return fx.CLUSTER; }
  async backups(): Promise<Backup[]> { this.guard("admin"); await lag(); return fx.BACKUPS; }
  async tokens(): Promise<Token[]> { this.guard("admin"); await lag(); return fx.TOKENS.map((t) => ({ ...t, deployment_id: this.deploymentId })); }
  async requestLog(): Promise<RequestLogLine[]> { this.guard("read"); await lag(); return fx.requestLog(); }

  async createToken(name: string, role: Role): Promise<Token> {
    this.guard("admin"); await lag();
    const secret = `big_${role[0]}_${Array.from({ length: 40 }, () => "abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789"[Math.floor(Math.random() * 55)]).join("")}`;
    return { id: `tok_${Date.now()}`, name, role, deployment_id: this.deploymentId,
      created_at: new Date().toISOString(), last_used_at: null, prefix: secret.slice(0, 10), secret };
  }
  async revokeToken(id: string): Promise<void> { this.guard("admin"); await lag(); }
}

/**
 * The fields of one table, by name. Built once per table and handed to the
 * validator: a linear scan per `field=value` pair turns a large file into an
 * O(pairs x fields) walk, which is the whole cost on a file of any size.
 */
export type FieldIndex = ReadonlyMap<string, FieldInfo>;

const indexes = new Map<string, FieldIndex>();

export function fieldIndex(table: TableInfo | undefined): FieldIndex {
  const t = table ?? fx.SCHEMA.tables[0];
  const hit = indexes.get(t.name);
  if (hit) return hit;
  const built: FieldIndex = new Map(t.fields.map((f) => [f.name, f]));
  indexes.set(t.name, built);
  return built;
}

/** One fact per line: `record_id field=value …`. Validated before a byte is sent. */
export function validateFactLine(text: string, fields: FieldIndex = fieldIndex(undefined)): { reason: string; code: string } | null {
  const t = text.trim();
  if (!t) return null;
  const parts = t.split(/\s+/);
  if (!/^\d+$/.test(parts[0])) return { code: "bad_record_id", reason: "the first column must be a record id (unsigned integer)" };
  if (parts.length < 2) return { code: "no_fields", reason: "a fact needs at least one `field=value` pair" };
  for (const p of parts.slice(1)) {
    if (!p.includes("=")) return { code: "bad_pair", reason: `\`${p}\` is not a \`field=value\` pair` };
    const [f, v] = p.split("=");
    const field = fields.get(f);
    if (!field) return { code: "unknown_field", reason: `no field \`${f}\` on this table` };
    if ((field.kind === "set" || field.kind === "mutex") && !/^".*"$/.test(v)) {
      return { code: "unquoted_value", reason: `\`${f}\` is a keyed field, so its value must be quoted: \`${f}="${v}"\`` };
    }
    if ((field.kind === "int" || field.kind === "signed_int") && !/^-?\d+$/.test(v)) {
      return { code: "bad_value", reason: `\`${f}\` is ${field.kind}, and \`${v}\` is not an integer` };
    }
    if (field.kind === "decimal") {
      if (!/^\d+(\.\d+)?$/.test(v)) return { code: "bad_value", reason: `\`${f}\` is decimal and unsigned; \`${v}\` is not` };
      const digits = v.split(".")[1]?.length ?? 0;
      if (digits > (field.scale ?? 0)) {
        return { code: "too_precise", reason: `\`${f}\` stores ${field.scale} decimal places, the value has ${digits}` };
      }
    }
  }
  return null;
}

/* ── control-plane (account-level) ──────────────────────────────────────── */

export const controlPlane = {
  async deployments(): Promise<Deployment[]> { await lag(); return fx.DEPLOYMENTS; },
  async deployment(id: string): Promise<Deployment> {
    await lag();
    const d = fx.DEPLOYMENTS.find((x) => x.id === id);
    if (!d) throw new ApiError(404, { code: "unknown_deployment", message: `no deployment \`${id}\`` });
    return d;
  },
  async createDeployment(input: { name: string; region: string; size: string; cluster: boolean }): Promise<Deployment> {
    await wait(1_200);
    return { id: `dep_${input.name}`, name: input.name, region: input.region,
      host: `big-99.${input.region}.bigdb.cloud`, version: "0.1.0",
      status: { live: true, ready: true }, health: "healthy", created_at: new Date().toISOString(),
      file_bytes: 4_096, page_count: 1, qpm: 0, p95_ms: 0,
      engines: { bitmap: 0, "bitmap+columnar": 0, columnar: 0 }, table_count: 0, cluster: input.cluster, role: "admin" };
  },
  async usage(): Promise<UsageRow[]> { await lag(); return fx.USAGE; },
  async invoices(): Promise<Invoice[]> { await lag(); return fx.INVOICES; },
  async members(): Promise<Member[]> { await lag(); return fx.MEMBERS; },
};

export function clientFor(deployment: Deployment) {
  return new BigClient(deployment.id, deployment.role);
}
