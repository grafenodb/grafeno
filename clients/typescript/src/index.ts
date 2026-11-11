/**
 * TypeScript client for the Grafeno HTTP API (`grafeno serve`).
 *
 * Field names follow the API as is (snake_case). Times are microseconds since
 * the Unix epoch in responses; requests also accept ISO 8601 strings.
 *
 * ```ts
 * const grafeno = new Grafeno({ url: "http://127.0.0.1:7810", token: process.env.GRAFENO_TOKEN });
 * const vault = grafeno.namespace("vault-123");
 * const { id } = await vault.createNode({ labels: ["Note"], props: { title: "Rust" }, text: "…" });
 * const context = await vault.context({ text: "borrow checker", budget: 800 });
 * ```
 */

/** This client's version (it has its own, apart from the engine's). */
export const CLIENT_VERSION = "0.2.0";

/** The versions of the HTTP API this client speaks. */
export const API_VERSIONS: readonly number[] = [1];

/**
 * What engines from before `GET /v1/about` (0.1.0, phases 1 to 15.2) can
 * do: everything but `settings`.
 */
export const LEGACY_CAPABILITIES: readonly string[] = [
  "namespaces", "nodes", "edges", "facts", "search.text", "search.vector",
  "recall", "context", "memory", "query", "query.explain", "batches", "traversal",
  "algorithms", "resolution", "match-rules", "extraction", "constraints",
  "communities", "summaries", "indexes", "changes", "changes.stream", "webhooks",
  "ingest", "export", "import", "import.csv", "import.graphml", "backup", "metrics",
  "accounts", "sessions", "keys", "roles", "grants", "policies", "audit",
  "cluster", "cluster.groups", "query.split",
];

/** What an engine is and can do (`GET /v1/about`). */
export interface About {
  product: string;
  /** The engine's version. */
  version: string;
  /** The API version it speaks, and every one it answers to. */
  api: { version: number; supported: number[] };
  /** The layout version of its database files. */
  layout: number;
  /** What it can do, by name (`query.split`, `audit`, …). */
  capabilities: string[];
  auth: { token: boolean; accounts: boolean; open: boolean; password_sign_in: boolean };
  /** This member's id and group, on a cluster. */
  cluster: { id: number; group: number } | null;
  /** True when the engine predates `/v1/about` and this was told from `/health`. */
  legacy?: boolean;
}

export type Id = string;
/** Microseconds since the Unix epoch, or an ISO 8601 date or date-time. */
export type TimeInput = number | string;
export type Props = Record<string, unknown>;
export type Direction = "out" | "in" | "both";

export interface Node {
  id: Id;
  labels: string[];
  props: Props;
  sources: Id[];
  created_at: number;
  updated_at: number;
  /** The API key (agent) that created it, when known. */
  recorded_by?: string;
}

/** How much a node matters and how often it is used, for ranking and decay. */
export interface MemoryState {
  /** From 0 to 1; 0.5 by default. */
  importance: number;
  accesses: number;
  /** Microseconds since the epoch; the last update until the first access. */
  last_access: number;
}

export interface NodeWithText extends Node {
  /** The indexed text, when the node has one. */
  text?: string;
  memory: MemoryState;
}

/** IDs in a namespace; the server takes up to 512 positions across both lists. */
export interface ReadManyInput {
  nodes?: Id[];
  edges?: Id[];
}

/** One position per ID, in the order asked; null when it does not exist. */
export interface ReadManyResult {
  nodes: (NodeWithText | null)[];
  edges: (Edge | null)[];
}

export interface Edge {
  id: Id;
  label: string;
  from: Id;
  to: Id;
  props: Props;
  sources: Id[];
  valid_from: number | null;
  valid_to: number | null;
  recorded_at: number;
  expired_at: number | null;
  /** The API key (agent) that recorded it, when known. */
  recorded_by?: string;
}

export type Role = "read" | "write" | "admin";

/** A role on a namespace, or on every name starting with a prefix (`team-*`, `*`). */
export interface Grant {
  namespace: string;
  role: Role;
}

/** An API key, as the server shows it (never its secret). */
export interface AccessKey {
  name: string;
  grants: Grant[];
  /** Every right on every namespace, and managing keys. */
  owner: boolean;
  /** May change only nodes and facts it recorded itself. */
  own_only: boolean;
  description?: string;
  /** Microseconds since the epoch; absent for the server's own token. */
  created_at?: number;
}

/** An account (14.1): a user who signs in, or a service account with keys. */
export interface Account {
  name: string;
  kind: "user" | "service";
  has_password: boolean;
  grants: Grant[];
  /** Roles granted to it (14.2). */
  roles: string[];
  superuser: boolean;
  /** `superuser`, under its name before 14.1. */
  owner: boolean;
  own_only: boolean;
  disabled: boolean;
  /** When it stops working (14.3), microseconds since the epoch. */
  expires_at?: number;
  /** What record policies compare with (14.4): `{ team: "red" }`. */
  attributes?: Record<string, unknown>;
  description?: string;
  created_at: number;
}

/** What a privilege is on (14.2). Namespaces may be patterns: `team-*`, `*`. */
export type PrivilegeObject =
  | { on: "server" }
  | { on: "namespace"; namespace: string }
  | { on: "label"; namespace: string; label: string; edges?: boolean }
  | { on: "property"; namespace: string; label: string; property: string };

/** An action on an object, granted or denied (a denial wins). */
export interface Privilege {
  action: string;
  object: PrivilegeObject;
  deny?: boolean;
}

/**
 * A limit to the records that match a condition (14.4): who holds the role
 * sees (and changes) only those. Policies narrow, like denials.
 */
export interface Policy {
  /** A namespace pattern. */
  namespace: string;
  /** Nodes with this label only; every node and edge when absent. */
  label?: string;
  applies?: "read" | "write" | "all";
  condition:
    | { kind: "recorded_by_caller" }
    | {
        kind: "property";
        key: string;
        equals: { from: "caller" } | { from: "attribute"; name: string } | { from: "value"; value: unknown };
      };
}

/** A security event of the audit log (14.5). */
export interface AuditEvent {
  seq: number;
  /** Microseconds since the epoch. */
  at: number;
  /** `account_created`, `signin_failed`, `lockout`, `key_rotated`, `refused`, … */
  kind: string;
  account?: string;
  /** The key or session behind the request: `key:name`, `session:id`, `token`. */
  via?: string;
  address?: string;
  target?: string;
  outcome: "ok" | "refused" | "failed";
  detail?: unknown;
}

/** A page of the audit log. */
export interface AuditPage {
  events: AuditEvent[];
  last: number;
  oldest: number;
  gap: boolean;
}

/** A role an administrator defined (14.2). */
export interface RoleDefinition {
  name: string;
  privileges: Privilege[];
  /** Built-in levels on namespace patterns (`read`, `write`, `admin`). */
  grants: Grant[];
  /** Roles it has. */
  roles: string[];
  /** Record policies (14.4). */
  policies?: Policy[];
  description?: string;
  created_at: number;
}

/** Every action by what it applies to, and the built-in roles. */
export interface PrivilegeCatalog {
  server: string[];
  namespace: string[];
  label: string[];
  property: string[];
  built_in: Record<string, unknown>;
}

/** What a key is narrowed to, inside its account's rights. */
export interface KeyScope {
  /** Namespace patterns it may reach (all of the account's when empty). */
  namespaces?: string[];
  read_only?: boolean;
}

/** The tokens of a session, from signing in or refreshing. */
export interface SessionTokens {
  access_token: string;
  /** Seconds until the access token expires. */
  expires_in: number;
  refresh_token: string;
  refresh_expires_in: number;
  session: string;
  account: Account;
}

export interface SessionInfo {
  id: string;
  account: string;
  created_at: number;
  expires_at: number;
  refresh_expires_at: number;
  refreshed_at?: number;
  address?: string;
  /** The session of the request that listed it. */
  current: boolean;
}

/** Whether a server is open, and whether its first account can be made. */
export interface AuthStatus {
  open: boolean;
  setup: boolean;
  password_sign_in: boolean;
}

export interface NodeInput {
  /** Only for upserts: your own id. */
  id?: Id;
  labels: string[];
  props?: Props;
  sources?: Id[];
  /** Indexed for full-text search. */
  text?: string;
  /** Indexed for vector search; one dimension per namespace. */
  embedding?: number[];
}

/** `text` or `embedding` set to `null` removes them. */
export interface NodePatch {
  /** Merged into the current properties; a `null` is stored as a value. */
  props?: Props;
  /** Properties to remove (engines with the `null_values` capability). */
  remove_props?: string[];
  add_labels?: string[];
  remove_labels?: string[];
  text?: string | null;
  embedding?: number[] | null;
  /** From 0 to 1. */
  importance?: number;
}

export interface EdgeInput {
  /** A node id, or in a batch a `$name` given earlier. */
  from: string;
  label: string;
  to: string;
  props?: Props;
  sources?: Id[];
  valid_from?: TimeInput;
  valid_to?: TimeInput;
}

export interface EdgeFilter {
  labels?: string[];
  valid_at?: TimeInput;
  known_at?: TimeInput;
}

export interface Neighbor {
  node: Id;
  edge: Edge;
}

export interface Path {
  nodes: Id[];
  edges: Id[];
}

export interface Subgraph {
  nodes: Node[];
  edges: Edge[];
}

export interface Hit {
  node: Id;
  score: number;
  /** Where a full-text index matched, when asked for. */
  fragments?: Fragment[];
}

/** A piece of a property around the words a full-text search matched. */
export interface Fragment {
  field: string;
  text: string;
  /** The matched words, as `[start, end)` offsets in characters of `text`. */
  matches: [number, number][];
}

export interface TextSearchOptions {
  limit?: number;
  /** A full-text index (its query language: "a phrase", +required, -excluded, prefix*, fuzzy~1, field:word). */
  index?: string;
  /** Fragments of what matched (with `index`). */
  fragments?: boolean;
}

export type SearchMode = "text" | "vector" | "hybrid";

/** Kept during a vector search's walk (engines with `search.vector.indexes`). */
export interface VectorFilter {
  /** Any of these labels. */
  labels?: string[];
  /** Each of these properties with this value. */
  props?: Record<string, unknown>;
}

export interface VectorSearchOptions {
  limit?: number;
  /** Candidates on layer 0. */
  ef?: number;
  /** A vector index; without one, the node embeddings. */
  index?: string;
  filter?: VectorFilter;
}

/** What `search` looks for (engines with `search.hybrid`). */
export interface SearchInput {
  text?: string;
  index?: string;
  embedding?: number[];
  /** A vector index for the embedding. */
  vector_index?: string;
  /** `text` (default), `vector`, or `hybrid` (both, fused). */
  mode?: SearchMode;
  /** `rank` (default, with `k`, 60) or `weighted` (with `text_weight`, 0.5). */
  fusion?: "rank" | "weighted";
  k?: number;
  text_weight?: number;
  limit?: number;
  /** Taken from each list before fusing. */
  candidates?: number;
  fragments?: boolean;
}

export interface SearchHit {
  node: Id;
  score: number;
  text_rank?: number;
  text_score?: number;
  vector_rank?: number;
  similarity?: number;
  fragments?: Fragment[];
}

export interface Expansion {
  depth?: number;
  direction?: Direction;
  filter?: EdgeFilter;
  decay?: number;
  seeds?: number;
}

export interface RecallInput {
  text?: string;
  embedding?: number[];
  limit?: number;
  labels?: string[];
  candidates?: number;
  /** `true` (default) expands with default options, `false` disables it. */
  expand?: boolean | Expansion;
  /** Also return archived memories. */
  include_archived?: boolean;
  /** `true` (default) weighs importance and recency, `false` ranks by relevance alone. */
  ranking?: boolean | Ranking;
  /** Record an access to each returned node. */
  touch?: boolean;
  /** A full-text index for `text`, with its query language. */
  index?: string;
  /** A vector index for `embedding`. */
  vector_index?: string;
  /** Use the words, the embedding, or both (default: what is given). */
  mode?: SearchMode;
  /** How text and vector results are put together. */
  fusion?: "rank" | "weighted";
  k?: number;
  text_weight?: number;
}

export interface Ranking {
  importance_weight?: number;
  recency_weight?: number;
  half_life_days?: number;
}

export type Signal =
  | { kind: "text"; rank: number; score: number }
  | { kind: "vector"; rank: number; similarity: number }
  | { kind: "graph"; seed: Id; hops: number }
  | { kind: "memory"; importance: number; recency: number };

export interface Recalled {
  node: Node;
  score: number;
  signals: Signal[];
}

export interface ContextInput extends RecallInput {
  budget?: number;
  fact_share?: number;
  max_facts?: number;
  facts?: EdgeFilter;
}

export interface ContextItem {
  node: Id;
  name: string;
  labels: string[];
  text: string;
  score: number;
  sources: Id[];
  truncated: boolean;
}

export interface ContextFact {
  edge: Id;
  from: Id;
  label: string;
  to: Id;
  text: string;
}

export interface Context {
  items: ContextItem[];
  facts: ContextFact[];
  tokens: number;
  /** Ready to paste into a prompt. */
  rendered: string;
}

export type BatchOp =
  | { op: "create_node"; ref?: string; node: NodeInput }
  | { op: "upsert_node"; ref?: string; node: NodeInput & { id: Id } }
  | { op: "update_node"; id: string; patch: NodePatch }
  | { op: "delete_node"; id: string }
  | { op: "create_edge"; edge: EdgeInput }
  | { op: "delete_edge"; id: Id }
  | { op: "delete_edges_from"; from: string; label?: string }
  | { op: "invalidate_edge"; id: Id; at: TimeInput }
  | { op: "retract_edge"; id: Id };

/** A page of a listing; pass `next` as `after` to get the following one. */
export interface Page<T> {
  items: T[];
  next: Id | null;
}

export interface ListOptions {
  label?: string;
  after?: Id;
  /** 1 to 1000; default 100. */
  limit?: number;
}

/** A namespace: created explicitly or by the first write, listed even when empty. */
export interface NamespaceInfo {
  name: string;
  nodes: number;
  edges: number;
  /** Microseconds since the epoch. */
  created_at: number;
  description?: string;
}

export interface NamespaceStats {
  name: string;
  nodes: number;
  edges: number;
  node_labels: Record<string, number>;
  edge_labels: Record<string, number>;
  texts: number;
  text_terms: number;
  vectors: number;
  vector_dimension: number | null;
}

export interface CheckReport {
  namespace: string;
  nodes: number;
  edges: number;
  texts: number;
  vectors: number;
  dangling_links: number;
  /** Empty when the namespace is consistent. */
  problems: string[];
}

export interface RebuildReport {
  namespace: string;
  nodes: number;
  edges: number;
  texts: number;
  vectors: number;
  dropped_edges: number;
  dropped_texts: number;
  dropped_vectors: number;
}

export interface DumpStats {
  namespaces: number;
  nodes: number;
  edges: number;
}

/** A graph algorithm the server can run (`GET /v1/algorithms`). */
export interface AlgorithmInfo {
  name: string;
  arguments: string;
  columns: string[];
  description: string;
}

/** How to run a graph algorithm over HTTP. */
export interface AlgorithmOptions {
  /** Arguments before the settings: node ids, `k`. */
  args?: unknown[];
  /** Projection and algorithm settings: `labels`, `edgeLabels`, `weight`, `direction`, … */
  settings?: Record<string, unknown>;
  /** Store the result in this node property (or `@embedding`) instead of returning rows. */
  write?: string;
  /** With `write`, the column to store (the one after `node` by default). */
  column?: string;
}

/** What a writing query changed. */
export interface WriteStats {
  nodes_created: number;
  nodes_deleted: number;
  edges_created: number;
  edges_deleted: number;
  edges_retracted: number;
  properties_set: number;
  labels_added: number;
  labels_removed: number;
}

/** The result of a GQ query. Nodes and edges come as their records, times as ISO strings. */
export interface PlanStep {
  step: number;
  operation: string;
  details: string;
  /** Rows after the step. */
  rows?: number;
  /** Records read: nodes, edges, adjacency and index entries. */
  reads?: number;
  time_ms?: number;
}

export interface QueryResult {
  columns: string[];
  rows: unknown[][];
  stats: WriteStats;
  /** What each step did, for statements that start with PROFILE. */
  profile?: PlanStep[];
  /**
   * On a cluster, a read split across the members: how many parts, and
   * how many ran on the member asked (its own, and those of members that
   * did not answer).
   */
  split?: { parts: number; here: number };
}

export interface BatchResult {
  results: Record<string, unknown>[];
  /** The ids given to each `$name`. */
  refs: Record<string, Id>;
}

export interface Duplicate {
  a: Node;
  b: Node;
  /** From 0 to 1; 1 is the same normalized name or the same email. */
  score: number;
  reasons: string[];
  /** Facts the two disagree on; a merge needs `force` to override them. */
  conflicts: Conflict[];
}

/** A disagreement between two nodes. */
export interface Conflict {
  /** `strong_key` and `labels` say they are different entities. */
  kind: "strong_key" | "labels" | "property" | "edge";
  /** The property or edge label (`labels` for labels). */
  key: string;
  kept: unknown;
  merged: unknown;
}

export interface MergeOptions {
  /** Merge in spite of conflicts: the kept node's values win. */
  force?: boolean;
  /** Only list the conflicts. */
  dry_run?: boolean;
  /** Edge labels a node has one of at a time (`born_in`). */
  functional?: string[];
  /** Identifying properties (default `email` and `external_id`, plus unique constraints). */
  strong_keys?: string[];
}

export interface MergeReport {
  /** The merge's record, for `undoMerge`. */
  id: Id;
  kept: Id;
  merged: Id;
  edges_moved: number;
  edges_dropped: number;
  /** Conflicts merged over with `force`. */
  conflicts: Conflict[];
}

/** A similarity measure for a field, with its settings. */
export type Measure =
  | { measure: "exact" | "edit" | "jaro" | "jaro_winkler" | "tokens" | "lcs" | "soundex" | "phonetic_es" }
  | { measure: "dice" | "jaccard"; n?: number }
  | { measure: "date"; days?: number }
  | { measure: "number"; tolerance?: number };

/** How one property compares in a match rule. */
export type FieldRule = { key: string; weight?: number } & Measure;

/** How to tell whether two nodes of a label are the same thing. */
export interface MatchRule {
  label: string;
  /** Properties that identify: equal decides same, different decides different. */
  strong_keys?: string[];
  fields?: FieldRule[];
  /** Weight of shared neighbors, when both sides have some (0: off). */
  neighbors?: number;
  /** From this score on: the same thing (0.9 by default). */
  same?: number;
  /** From this score on: to review (0.75 by default). */
  review?: number;
}

export interface FieldScore {
  key: string;
  measure: string;
  record: unknown;
  node: unknown;
  similarity: number;
  weight: number;
}

/** A node compared with a record, and why it scored what it did. */
export interface Match {
  node: Id;
  score: number;
  decision: "same" | "review" | "different";
  /** The strong key that decided, if one did. */
  decided_by: string | null;
  fields: FieldScore[];
  neighbors: number | null;
}

export interface FuzzyHit {
  node: Id;
  score: number;
}

/** What an extraction run (or an agent's mentions) did. */
export interface ExtractReport {
  read: number;
  unchanged: number;
  mentions: number;
  linked: number;
  created: number;
  queued: number;
  found: Record<string, number>;
}

/** Something extraction could not decide on its own. */
export interface ReviewItem {
  key: string;
  kind: "phrase" | "ambiguous";
  value: string;
  occurrences: { source: Id; start: number; end: number; surface: string }[];
  candidates: Id[];
}

/** A recorded merge (the snapshots it keeps for undo are left loosely typed). */
export interface MergeRecord {
  id: Id;
  kept: Id;
  merged: Id;
  /** Microseconds since the epoch. */
  at: number;
  by: string | null;
  forced: boolean;
  conflicts: Conflict[];
  kept_before: { node: Node; text: string | null };
  merged_before: { node: Node; text: string | null };
  moved: Edge[];
  dropped: Edge[];
  undone_at: number | null;
  undone_by: string | null;
}

export interface WeakMemory {
  node: Node;
  /** importance × recency × use, from 0 to 1. */
  strength: number;
  memory: MemoryState;
}

export interface ConsolidationGroup {
  /** The entity the memories are about. */
  entity: Node;
  memories: Node[];
}

export interface SummaryInput {
  text: string;
  /** The memories summarized. */
  sources: Id[];
  /** The entity they are about. */
  about?: Id;
  name?: string;
  /** Default: `["Summary"]`. */
  labels?: string[];
  embedding?: number[];
  /** Archive the sources (default true). */
  archive_sources?: boolean;
}

/**
 * How fresh a read must be on a cluster: `local` (default) reads what the
 * member has applied, `bounded` sends it to the leader when the member
 * lags far behind, `linearizable` sees every write acknowledged before it
 * (through the leader). A single server ignores it.
 */
export type Consistency = "local" | "bounded" | "linearizable";

export interface GrafenoOptions {
  /**
   * Base URL, such as `http://127.0.0.1:7810`. For a cluster, several
   * members' URLs: the client uses one and moves to the next when it does
   * not answer.
   */
  url: string | string[];
  /** Bearer token, if the server was started with one. */
  token?: string;
  /** The consistency of every request (default `local`). */
  consistency?: Consistency;
  /**
   * Read your own writes on any member (default true): each request asks
   * the member to have applied the last position this client saw.
   */
  readYourWrites?: boolean;
  /**
   * Spread requests over the members (default false): each request starts
   * on the next one. Followers forward writes to the leader, and with
   * `readYourWrites` reads still see this client's writes.
   */
  spread?: boolean;
  /** A custom `fetch`, for tests or other runtimes. */
  fetch?: typeof fetch;
  /**
   * The tokens of a session signed in before (kept by the application):
   * the client uses its access token and renews it when it expires.
   */
  session?: Pick<SessionTokens, "access_token" | "refresh_token">;
  /** Called with new tokens after signing in or renewing, and `null` when the session ends. */
  onSession?: (tokens: SessionTokens | null) => void;
}

/** Options of one GQ statement. */
export interface QueryOptions {
  /**
   * A write sent again with the same key (a retry after a lost answer)
   * runs once: later sends return the first result.
   */
  idempotencyKey?: string;
  /** The consistency of this statement, over the client's. */
  consistency?: Consistency;
  /**
   * On a cluster, `on` splits a read across the members whenever its shape
   * allows, `off` never; `auto` (default) when it is costly enough.
   */
  split?: "auto" | "on" | "off";
}

/** An error returned by the server (or a network failure, with status 0). */
export class GrafenoError extends Error {
  readonly status: number;
  readonly code: string;
  /** Other fields of the error (`conflicts` of a refused merge). */
  readonly details: Record<string, unknown>;

  constructor(status: number, code: string, message: string, details: Record<string, unknown> = {}) {
    super(message);
    this.name = "GrafenoError";
    this.status = status;
    this.code = code;
    this.details = details;
  }
}

export class Grafeno {
  readonly #urls: string[];
  #current = 0;
  #position = 0;
  #token: string | undefined;
  #refresh: string | undefined;
  readonly #onSession: ((tokens: SessionTokens | null) => void) | undefined;
  readonly #consistency: Consistency | undefined;
  readonly #readYourWrites: boolean;
  readonly #spread: boolean;
  readonly #fetch: typeof fetch;
  #about: About | undefined;

  constructor(options: GrafenoOptions) {
    const urls = Array.isArray(options.url) ? options.url : [options.url];
    this.#urls = urls.map((url) => url.replace(/\/+$/, "")).filter((url) => url !== "");
    if (this.#urls.length === 0) throw new Error("Grafeno needs at least one URL");
    this.#token = options.session?.access_token ?? options.token;
    this.#refresh = options.session?.refresh_token;
    this.#onSession = options.onSession;
    this.#consistency = options.consistency;
    this.#readYourWrites = options.readYourWrites ?? true;
    this.#spread = options.spread ?? false;
    // Browsers refuse `fetch` called as a method of another object
    // ("Illegal invocation"), so the global one is bound to `globalThis`.
    this.#fetch = options.fetch ?? fetch.bind(globalThis);
  }

  /** The member this client talks to now. */
  get url(): string {
    return this.#urls[this.#current];
  }

  /** The latest cluster position this client saw (0 on a single server). */
  get position(): number {
    return this.#position;
  }

  /** The cluster as the current member sees it: role, leader, positions, voters (owners only). */
  cluster(): Promise<Record<string, unknown>> {
    return this.request("GET", "/v1/cluster");
  }

  /** Operations on one namespace (a user, organization or agent). */
  namespace(name: string): Namespace {
    return new Namespace(this, name);
  }

  health(): Promise<{ status: string; version: string; layout?: number }> {
    return this.request("GET", "/health");
  }

  /**
   * What the engine is and can do (`GET /v1/about`, public). An engine from
   * before it answers 404: the client then tells what it can from `/health`
   * (`legacy: true`, the capabilities of that time).
   */
  async about(): Promise<About> {
    try {
      this.#about = await this.request<About>("GET", "/v1/about");
    } catch (error) {
      if (!(error instanceof GrafenoError) || error.status !== 404) throw error;
      const health = await this.health();
      this.#about = {
        product: "grafeno",
        version: health.version,
        api: { version: 1, supported: [1] },
        layout: health.layout ?? 0,
        capabilities: [...LEGACY_CAPABILITIES],
        auth: { token: true, accounts: true, open: false, password_sign_in: true },
        cluster: null,
        legacy: true,
      };
    }
    return this.#about;
  }

  /**
   * Checks the engine before using it: reads `about()` and fails with
   * `incompatible` (status 0) if it speaks no API version this client
   * does. Afterwards `supports()` answers from what it said.
   */
  async connect(): Promise<About> {
    const about = await this.about();
    const shared = about.api.supported.filter((version) => API_VERSIONS.includes(version));
    if (shared.length === 0) {
      throw new GrafenoError(
        0,
        "incompatible",
        `the engine ${about.version} speaks API ${about.api.supported.join(", ")}; this client speaks ${API_VERSIONS.join(", ")}`,
        { engine: about.version, engineApi: about.api.supported, clientApi: [...API_VERSIONS] },
      );
    }
    return about;
  }

  /** What the engine said about itself, once `about()` or `connect()` ran. */
  get engine(): About | undefined {
    return this.#about;
  }

  /**
   * Whether the engine can do `capability` (`"audit"`, `"query.split"`, …),
   * from `about()` or `connect()`; false before either ran.
   */
  supports(capability: string): boolean {
    return this.#about?.capabilities.includes(capability) ?? false;
  }

  /** The settings the server runs with, without secrets (superusers; capability `settings`). */
  settings(): Promise<Record<string, unknown>> {
    return this.request("GET", "/v1/settings");
  }

  /** Every namespace that has nodes, in name order. */
  namespaces(): Promise<NamespaceInfo[]> {
    return this.request("GET", "/v1/namespaces");
  }

  /** Creates an empty namespace; fails with status 409 (`conflict`) if it exists. */
  createNamespace(name: string, description?: string): Promise<NamespaceInfo> {
    return this.request("POST", "/v1/namespaces", { name, description });
  }

  /** One namespace; fails with status 404 if it does not exist. */
  getNamespace(name: string): Promise<NamespaceInfo> {
    return this.request("GET", `/v1/namespaces/${encodeURIComponent(name)}`);
  }

  /** Sets the description of a namespace (`null` removes it). */
  describeNamespace(name: string, description: string | null): Promise<NamespaceInfo> {
    return this.request("PATCH", `/v1/namespaces/${encodeURIComponent(name)}`, { description });
  }

  /** Deletes a namespace and everything in it. It cannot be undone. */
  dropNamespace(name: string): Promise<{ nodes: number; edges: number }> {
    return this.request("DELETE", `/v1/namespaces/${encodeURIComponent(name)}`);
  }

  /** Who this client is: the server's token, or an account (by key or session) with its grants. */
  whoami(): Promise<AccessKey & Partial<Account> & { via?: { kind: "key" | "session"; name: string }; scope?: KeyScope }> {
    return this.request("GET", "/v1/whoami");
  }

  /** Whether the server is open, and whether its first account can be made. */
  authStatus(): Promise<AuthStatus> {
    return this.request("GET", "/v1/auth/status");
  }

  #adopt(tokens: SessionTokens): SessionTokens {
    this.#token = tokens.access_token;
    this.#refresh = tokens.refresh_token;
    this.#onSession?.(tokens);
    return tokens;
  }

  /** Signs in with a user's name and password; the client then uses the session. */
  async signIn(name: string, password: string): Promise<SessionTokens> {
    this.#token = undefined;
    return this.#adopt(await this.request<SessionTokens>("POST", "/v1/auth/signin", { name, password }));
  }

  /** On an open server with no accounts, makes the first one (a superuser) and signs it in. */
  async setup(name: string, password: string): Promise<SessionTokens> {
    return this.#adopt(await this.request<SessionTokens>("POST", "/v1/auth/setup", { name, password }));
  }

  /** New tokens for the session (both replaced); done by itself when the access token expires. */
  async refreshSession(): Promise<SessionTokens> {
    if (!this.#refresh) throw new GrafenoError(401, "unauthorized", "not signed in");
    const refresh_token = this.#refresh;
    this.#token = undefined;
    try {
      return this.#adopt(await this.request<SessionTokens>("POST", "/v1/auth/refresh", { refresh_token }));
    } catch (error) {
      this.#refresh = undefined;
      this.#onSession?.(null);
      throw error;
    }
  }

  /** Ends this client's session. */
  async signOut(): Promise<void> {
    try {
      await this.request("POST", "/v1/auth/signout");
    } finally {
      this.#token = undefined;
      this.#refresh = undefined;
      this.#onSession?.(null);
    }
  }

  /** The signed-in account's sessions (a superuser's: `account`'s, or `*` for all). */
  sessions(account?: string): Promise<SessionInfo[]> {
    return this.request("GET", `/v1/auth/sessions${account ? `?account=${encodeURIComponent(account)}` : ""}`);
  }

  /** Ends one of the account's sessions (a superuser: anyone's). */
  async endSession(id: string): Promise<void> {
    await this.request("DELETE", `/v1/auth/sessions/${encodeURIComponent(id)}`);
  }

  /** Changes the signed-in user's password; its other sessions end. */
  async changePassword(currentPassword: string, newPassword: string): Promise<void> {
    await this.request("POST", "/v1/auth/password", { current_password: currentPassword, new_password: newPassword });
  }

  /** Every account (superusers). */
  accounts(): Promise<Account[]> {
    return this.request("GET", "/v1/accounts");
  }

  /** Makes a user (with a password) or a service account (superusers). */
  createAccount(account: {
    name: string;
    kind: "user" | "service";
    password?: string;
    grants?: Grant[];
    roles?: string[];
    superuser?: boolean;
    own_only?: boolean;
    description?: string;
  }): Promise<Account> {
    return this.request("POST", "/v1/accounts", account);
  }

  /** Changes an account: password, grants, status (superusers). */
  changeAccount(
    name: string,
    change: {
      password?: string;
      grants?: Grant[];
      roles?: string[];
      superuser?: boolean;
      own_only?: boolean;
      disabled?: boolean;
      /** Microseconds or an ISO 8601 date; `null` for never. */
      expires_at?: number | string | null;
      /** Replaces the attributes record policies compare with. */
      attributes?: Record<string, unknown>;
      description?: string | null;
    },
  ): Promise<Account> {
    return this.request("PATCH", `/v1/accounts/${encodeURIComponent(name)}`, change);
  }

  /** Deletes an account with its keys and sessions (superusers). */
  async deleteAccount(name: string): Promise<void> {
    await this.request("DELETE", `/v1/accounts/${encodeURIComponent(name)}`);
  }

  /** Audit events after `since` (the `audit` privilege), 14.5. */
  audit(options: { since?: number; limit?: number } = {}): Promise<AuditPage> {
    const query = new URLSearchParams();
    if (options.since !== undefined) query.set("since", String(options.since));
    if (options.limit !== undefined) query.set("limit", String(options.limit));
    const suffix = query.size > 0 ? `?${query}` : "";
    return this.request("GET", `/v1/audit${suffix}`);
  }

  /** Every privilege by what it applies to (14.2). */
  privilegeCatalog(): Promise<PrivilegeCatalog> {
    return this.request("GET", "/v1/privileges");
  }

  /** The roles (`manage_roles`). */
  roles(): Promise<RoleDefinition[]> {
    return this.request("GET", "/v1/admin/roles");
  }

  /** One role (`manage_roles`). */
  role(name: string): Promise<RoleDefinition> {
    return this.request("GET", `/v1/admin/roles/${encodeURIComponent(name)}`);
  }

  /** Defines or replaces a role; cycles are refused (`manage_roles`). */
  putRole(
    name: string,
    role: { privileges?: Privilege[]; grants?: Grant[]; roles?: string[]; policies?: Policy[]; description?: string },
  ): Promise<RoleDefinition> {
    return this.request("PUT", `/v1/admin/roles/${encodeURIComponent(name)}`, role);
  }

  /** Removes a role; every account and role that had it loses it. */
  async deleteRole(name: string): Promise<void> {
    await this.request("DELETE", `/v1/admin/roles/${encodeURIComponent(name)}`);
  }

  /**
   * Grants privileges (actions such as `read`, or the levels `reader`,
   * `editor`, `admin`) on an object to roles; `deny` denies them instead.
   */
  grant(privileges: string[], on: PrivilegeObject, to: string[], options: { deny?: boolean } = {}): Promise<QueryResult> {
    return this.request("POST", "/v1/admin/grants", { privileges, on, to, deny: options.deny ?? false });
  }

  /** Revokes privileges on an object from roles: grants, denials or both. */
  revoke(
    privileges: string[],
    on: PrivilegeObject,
    from: string[],
    which: "grants" | "denials" | "both" = "both",
  ): Promise<QueryResult> {
    return this.request("POST", "/v1/admin/grants/revoke", { privileges, on, from, which });
  }

  /** Gives roles to accounts (`manage_accounts`) or roles (`manage_roles`). */
  grantRoles(roles: string[], to: string[]): Promise<QueryResult> {
    return this.request("POST", "/v1/admin/grants/roles", { roles, to });
  }

  /** Takes roles from accounts or roles. */
  revokeRoles(roles: string[], from: string[]): Promise<QueryResult> {
    return this.request("POST", "/v1/admin/grants/roles/revoke", { roles, from });
  }

  /** An account's or a role's privileges, each with the role it comes from. */
  privilegesOf(holder: string): Promise<QueryResult> {
    return this.request("GET", `/v1/admin/grants?holder=${encodeURIComponent(holder)}`);
  }

  /** Replaces a key's secret; the new one is returned only here, the old one stops working. */
  rotateKey(name: string): Promise<{ key: AccessKey; secret: string }> {
    return this.request("POST", `/v1/keys/${encodeURIComponent(name)}/rotate`);
  }

  /** The API keys: every one for superusers, the account's own for others. */
  keys(): Promise<AccessKey[]> {
    return this.request("GET", "/v1/keys");
  }

  /** Creates an API key; its secret is returned only here. */
  createKey(key: {
    name: string;
    /** The account it belongs to (the caller's when absent). */
    account?: string;
    grants?: Grant[];
    owner?: boolean;
    own_only?: boolean;
    scope?: KeyScope;
    expires_in_days?: number;
    description?: string;
  }): Promise<{
    key: AccessKey;
    secret: string;
  }> {
    return this.request("POST", "/v1/keys", key);
  }

  /** Revokes an API key: its secret stops working at once. */
  async revokeKey(name: string): Promise<void> {
    await this.request("DELETE", `/v1/keys/${encodeURIComponent(name)}`);
  }

  /** The graph algorithms `CALL algo.*` and `algorithm()` can run. */
  algorithms(): Promise<AlgorithmInfo[]> {
    return this.request("GET", "/v1/algorithms");
  }

  /** A JSON Lines export of `namespaces` (all of them by default). */
  /** A consistent copy of the whole database file, taken while the server keeps serving (owners only). */
  async backup(): Promise<ArrayBuffer> {
    return this.#send("GET", "/v1/backup").then((response) => response.arrayBuffer());
  }

  async export(namespaces: string[] = []): Promise<string> {
    const query = namespaces.length ? `?ns=${encodeURIComponent(namespaces.join(","))}` : "";
    return this.#send("GET", `/v1/export${query}`).then((response) => response.text());
  }

  /** Imports a JSON Lines export in one transaction: all of it or nothing. */
  /**
   * Imports an export. `bulk` loads it in chunks: faster for large exports,
   * but an error keeps the chunks before it.
   */
  async import(dump: string, options: { bulk?: boolean } = {}): Promise<DumpStats> {
    const path = options.bulk ? "/v1/import?bulk=true" : "/v1/import";
    const response = await this.#send("POST", path, dump, "application/x-ndjson");
    return (await response.json()) as DumpStats;
  }

  /** Sends a request and parses the JSON response. */
  async request<T>(method: string, path: string, body?: unknown, extra: Record<string, string> = {}): Promise<T> {
    const response = await this.#send(
      method,
      path,
      body === undefined ? undefined : JSON.stringify(body),
      "application/json",
      extra,
    );
    const text = await response.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  /** Sends a request; throws `GrafenoError` unless the response is a success. */
  /** Sends a request; an expired session is renewed once, and the request sent again. */
  async #send(
    method: string,
    path: string,
    body?: string,
    contentType?: string,
    extra: Record<string, string> = {},
  ): Promise<Response> {
    try {
      return await this.#sendOnce(method, path, body, contentType, extra);
    } catch (error) {
      if (error instanceof GrafenoError && error.code === "session_expired" && this.#refresh && !path.startsWith("/v1/auth/")) {
        await this.refreshSession();
        return this.#sendOnce(method, path, body, contentType, extra);
      }
      throw error;
    }
  }

  async #sendOnce(
    method: string,
    path: string,
    body?: string,
    contentType?: string,
    extra: Record<string, string> = {},
  ): Promise<Response> {
    const headers: Record<string, string> = {};
    if (body !== undefined && contentType) headers["content-type"] = contentType;
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    if (this.#consistency) headers["x-grafeno-consistency"] = this.#consistency;
    if (this.#readYourWrites && this.#position > 0) headers["x-grafeno-after"] = String(this.#position);
    Object.assign(headers, extra);
    // The member in use first (the next one when spreading), then the
    // others in turn.
    if (this.#spread) this.#current = (this.#current + 1) % this.#urls.length;
    let response: Response | undefined;
    const failures: string[] = [];
    for (let step = 0; step < this.#urls.length && response === undefined; step++) {
      const at = (this.#current + step) % this.#urls.length;
      try {
        response = await this.#fetch(this.#urls[at] + path, { method, headers, body });
        this.#current = at;
      } catch (error) {
        failures.push(`${this.#urls[at]}: ${String(error)}`);
      }
    }
    if (response === undefined) {
      throw new GrafenoError(0, "network", `request to Grafeno failed: ${failures.join("; ")}`);
    }
    const seen = Number(response.headers.get("x-grafeno-position"));
    if (Number.isSafeInteger(seen) && seen > this.#position) this.#position = seen;
    if (!response.ok) {
      const text = await response.text();
      let error: ({ code?: string; message?: string } & Record<string, unknown>) | undefined;
      try {
        error = (JSON.parse(text) as { error?: { code?: string; message?: string } & Record<string, unknown> }).error;
      } catch {
        error = undefined;
      }
      const { code, message, ...details } = error ?? {};
      throw new GrafenoError(response.status, code ?? "http", message ?? (text || response.statusText), details);
    }
    return response;
  }
}

function listQuery(options: ListOptions): string {
  const params = new URLSearchParams();
  if (options.label !== undefined) params.set("label", options.label);
  if (options.after !== undefined) params.set("after", options.after);
  if (options.limit !== undefined) params.set("limit", String(options.limit));
  return params.size ? `?${params}` : "";
}

/** An explicit transaction, as `begin` reports it. */
export interface TransactionInfo {
  id: string;
  read_only: boolean;
  started_at: string;
  timeout_secs: number;
  statements: number;
  /** Why it failed, when a statement in it did: only `rollback` ends it. */
  failed: string | null;
}

export interface BeginOptions {
  /** One snapshot, no writes. */
  readOnly?: boolean;
  /** Seconds without a request before the server rolls it back (at most its own). */
  timeoutSecs?: number;
}

/**
 * An explicit transaction (engines with the `transactions` capability):
 * statements across calls, then `commit` or `rollback`. A writing one holds
 * the server's writer while it is open, so end it promptly.
 */
export class Transaction {
  readonly #client: Grafeno;
  readonly #base: string;
  readonly info: TransactionInfo;

  constructor(client: Grafeno, base: string, info: TransactionInfo) {
    this.#client = client;
    this.#base = `${base}/transactions/${encodeURIComponent(info.id)}`;
    this.info = info;
  }

  get id(): string {
    return this.info.id;
  }

  /** Runs one GQ statement in the transaction. */
  query(query: string, params: Record<string, unknown> = {}): Promise<QueryResult> {
    return this.#client.request("POST", `${this.#base}/query`, { query, params });
  }

  /** Its current state on the server. */
  status(): Promise<TransactionInfo> {
    return this.#client.request("GET", this.#base);
  }

  /** Makes everything it did durable and visible, at once. */
  async commit(): Promise<void> {
    await this.#client.request("POST", `${this.#base}/commit`);
  }

  /** Discards everything it did. */
  async rollback(): Promise<void> {
    await this.#client.request("POST", `${this.#base}/rollback`);
  }
}

export class Namespace {
  readonly #client: Grafeno;
  readonly #base: string;
  readonly name: string;

  constructor(client: Grafeno, name: string) {
    this.#client = client;
    this.name = name;
    this.#base = `/v1/${encodeURIComponent(name)}`;
  }

  #call<T>(method: string, path: string, body?: unknown): Promise<T> {
    return this.#client.request<T>(method, this.#base + path, body);
  }

  // Nodes

  /** Creates a node, or upserts it when `input.id` is set. */
  createNode(input: NodeInput): Promise<{ id: Id; created: boolean }> {
    return this.#call("POST", "/nodes", input);
  }

  /** Creates the node with this id or replaces it entirely (edges are kept). */
  putNode(id: Id, input: Omit<NodeInput, "id">): Promise<{ id: Id; created: boolean }> {
    return this.#call("PUT", `/nodes/${encodeURIComponent(id)}`, input);
  }

  getNode(id: Id): Promise<NodeWithText> {
    return this.#call("GET", `/nodes/${encodeURIComponent(id)}`);
  }

  /** Reads nodes and edges in one snapshot and one request. */
  readMany(input: ReadManyInput): Promise<ReadManyResult> {
    return this.#call("POST", "/read-many", input);
  }

  /** A page of nodes in creation order, only those with `label` if given. */
  listNodes(options: ListOptions = {}): Promise<Page<Node>> {
    return this.#call("GET", `/nodes${listQuery(options)}`);
  }

  /** A page of edges (retracted ones included), only those with `label` if given. */
  listEdges(options: ListOptions = {}): Promise<Page<Edge>> {
    return this.#call("GET", `/edges${listQuery(options)}`);
  }

  /** Every node, following the pages. */
  async *allNodes(options: Omit<ListOptions, "after"> = {}): AsyncGenerator<Node> {
    let after: Id | undefined;
    do {
      const page = await this.listNodes({ ...options, after });
      yield* page.items;
      after = page.next ?? undefined;
    } while (after);
  }

  stats(): Promise<NamespaceStats> {
    return this.#call("GET", "/stats");
  }

  /** Checks that indexes and counters agree with the records. */
  check(): Promise<CheckReport> {
    return this.#call("GET", "/check");
  }

  /** Rebuilds the namespace's indexes and counters from its records. */
  rebuild(): Promise<RebuildReport> {
    return this.#call("POST", "/rebuild");
  }

  patchNode(id: Id, patch: NodePatch): Promise<NodeWithText> {
    return this.#call("PATCH", `/nodes/${encodeURIComponent(id)}`, patch);
  }

  /** Deletes the node, its edges, text and embedding. */
  async deleteNode(id: Id): Promise<void> {
    await this.#call("DELETE", `/nodes/${encodeURIComponent(id)}`);
  }

  neighbors(
    id: Id,
    options: { direction?: Direction; labels?: string[]; validAt?: TimeInput; knownAt?: TimeInput } = {},
  ): Promise<Neighbor[]> {
    const params = new URLSearchParams();
    if (options.direction) params.set("direction", options.direction);
    if (options.labels?.length) params.set("label", options.labels.join(","));
    if (options.validAt !== undefined) params.set("valid_at", String(options.validAt));
    if (options.knownAt !== undefined) params.set("known_at", String(options.knownAt));
    const query = params.size ? `?${params}` : "";
    return this.#call("GET", `/nodes/${encodeURIComponent(id)}/neighbors${query}`);
  }

  // Edges

  createEdge(input: EdgeInput): Promise<{ id: Id }> {
    return this.#call("POST", "/edges", input);
  }

  getEdge(id: Id): Promise<Edge> {
    return this.#call("GET", `/edges/${encodeURIComponent(id)}`);
  }

  async deleteEdge(id: Id): Promise<void> {
    await this.#call("DELETE", `/edges/${encodeURIComponent(id)}`);
  }

  /** The fact stopped being true at `at`; its history is kept. */
  invalidateEdge(id: Id, at: TimeInput): Promise<Edge> {
    return this.#call("POST", `/edges/${encodeURIComponent(id)}/invalidate`, { at });
  }

  /** The fact was a mistake; it stays visible to `known_at` queries. */
  retractEdge(id: Id): Promise<Edge> {
    return this.#call("POST", `/edges/${encodeURIComponent(id)}/retract`);
  }

  // Traversals

  async kHop(
    start: Id,
    options: { depth?: number; direction?: Direction; filter?: EdgeFilter } = {},
  ): Promise<{ node: Id; hops: number }[]> {
    return this.#call("POST", "/k-hop", { start, ...options });
  }

  async path(
    from: Id,
    to: Id,
    options: { max_depth?: number; direction?: Direction; filter?: EdgeFilter } = {},
  ): Promise<Path | null> {
    const result = await this.#call<{ path: Path | null }>("POST", "/path", { from, to, ...options });
    return result.path;
  }

  subgraph(
    seeds: Id[],
    options: { depth?: number; direction?: Direction; filter?: EdgeFilter } = {},
  ): Promise<Subgraph> {
    return this.#call("POST", "/subgraph", { seeds, ...options });
  }

  /** Depth-one neighborhoods of up to eight roots, in one snapshot. */
  neighborhoods(
    seeds: Id[],
    options: { direction?: Direction; filter?: EdgeFilter } = {},
  ): Promise<Subgraph[]> {
    return this.#call("POST", "/neighborhoods", { seeds, ...options });
  }

  // Retrieval

  /** Text search: the node text, or a full-text index with `index`. */
  searchText(query: string, options: number | TextSearchOptions = 10): Promise<Hit[]> {
    const { limit = 10, ...rest } = typeof options === "number" ? { limit: options } : options;
    return this.#call("POST", "/search/text", { query, limit, ...rest });
  }

  /** Text, vector or hybrid search, as `input.mode` says. */
  search(input: SearchInput): Promise<SearchHit[]> {
    return this.#call("POST", "/search", input);
  }

  searchVector(
    embedding: number[],
    options: number | VectorSearchOptions = 10,
    ef?: number,
  ): Promise<Hit[]> {
    const rest = typeof options === "number" ? { limit: options, ef } : options;
    return this.#call("POST", "/search/vector", { embedding, ...rest });
  }

  /** Several vector searches at once: a list of hits per embedding. */
  searchVectors(embeddings: number[][], options: VectorSearchOptions = {}): Promise<Hit[][]> {
    return this.#call("POST", "/search/vector", { embeddings, ...options });
  }

  /** The nodes nearest to a node's own vector, itself left out. */
  similar(node: Id, options: VectorSearchOptions = {}): Promise<Hit[]> {
    return this.#call("POST", "/similar", { node, ...options });
  }

  /** Hybrid recall: full text and vectors fused, then graph expansion. */
  recall(input: RecallInput): Promise<Recalled[]> {
    return this.#call("POST", "/recall", input);
  }

  /** Recalled nodes and facts packed into a token budget. */
  context(input: ContextInput): Promise<Context> {
    return this.#call("POST", "/context", input);
  }

  /**
   * Runs one GQ statement, such as
   * `MATCH (p:Person)-[:works_at]->(c) RETURN p.name, c.name`. Statements
   * with writes run in one transaction.
   */
  query(query: string, params: Record<string, unknown> = {}, options: QueryOptions = {}): Promise<QueryResult> {
    const extra: Record<string, string> = {};
    if (options.idempotencyKey !== undefined) extra["idempotency-key"] = options.idempotencyKey;
    if (options.consistency !== undefined) extra["x-grafeno-consistency"] = options.consistency;
    if (options.split !== undefined) extra["x-grafeno-split"] = options.split;
    return this.#client.request("POST", this.#base + "/query", { query, params }, extra);
  }

  /**
   * Runs a graph algorithm (`pageRank`, `louvain`, `shortestPath`, …). Rows hold
   * nodes as `{id, labels, name}`; with `write`, one row with the nodes written.
   */
  algorithm(name: string, options: AlgorithmOptions = {}): Promise<QueryResult> {
    return this.#call("POST", `/algo/${encodeURIComponent(name)}`, options);
  }

  /** Applies every operation in one transaction: all of them or none. */
  batch(ops: BatchOp[]): Promise<BatchResult> {
    return this.#call("POST", "/batch", { ops });
  }

  /** Begins an explicit transaction (16.4). */
  async begin(options: BeginOptions = {}): Promise<Transaction> {
    const info = await this.#call<TransactionInfo>("POST", "/transactions", {
      read_only: options.readOnly ?? false,
      ...(options.timeoutSecs === undefined ? {} : { timeout_secs: options.timeoutSecs }),
    });
    return new Transaction(this.#client, this.#base, info);
  }

  /**
   * Runs `work` in an explicit transaction: committed when it resolves,
   * rolled back when it throws (the error is thrown again).
   */
  async transaction<T>(work: (tx: Transaction) => Promise<T>, options: BeginOptions = {}): Promise<T> {
    const tx = await this.begin(options);
    let value: T;
    try {
      value = await work(tx);
    } catch (error) {
      await tx.rollback().catch(() => undefined);
      throw error;
    }
    await tx.commit();
    return value;
  }

  // Memory

  /** Nodes with this name or alias, ignoring case, accents and punctuation. */
  resolve(name: string, label?: string): Promise<Node[]> {
    return this.#call("GET", `/resolve${queryString({ name, label })}`);
  }

  /** Likely duplicates, best first: by name, and by meaning when `vectors` is not false. */
  duplicates(options: { label?: string; min_score?: number; vectors?: boolean; limit?: number } = {}): Promise<Duplicate[]> {
    return this.#call("GET", `/duplicates${queryString(options)}`);
  }

  /**
   * Folds `merge` into `keep`: edges move, labels and missing properties join, `merge` is deleted.
   * Fails with status 409 (`conflict`, the list in `error.details.conflicts`) when they disagree,
   * unless `force`.
   */
  merge(keep: Id, merge: Id, options: Omit<MergeOptions, "dry_run"> = {}): Promise<MergeReport> {
    return this.#call("POST", "/merge", { keep, merge, ...options });
  }

  /** What two nodes disagree on, without merging. */
  async mergeConflicts(keep: Id, merge: Id, options: Omit<MergeOptions, "dry_run" | "force"> = {}): Promise<Conflict[]> {
    const result: { conflicts: Conflict[] } = await this.#call("POST", "/merge", { keep, merge, ...options, dry_run: true });
    return result.conflicts;
  }

  /** Recorded merges, newest first; with `node`, those that kept or merged it. */
  merges(options: { node?: Id; limit?: number } = {}): Promise<MergeRecord[]> {
    return this.#call("GET", `/merges${queryString(options)}`);
  }

  /** Undoes a merge: the merged node and its facts come back. `force` if the kept node changed since. */
  undoMerge(id: Id, force = false): Promise<MergeRecord> {
    return this.#call("POST", `/merges/${encodeURIComponent(id)}/undo`, { force });
  }

  /** Nodes whose property looks like `text`, through the fuzzy index on it (`CREATE FUZZY INDEX`). */
  fuzzy(label: string, key: string, text: string, options: { limit?: number; measure?: string } = {}): Promise<FuzzyHit[]> {
    return this.#call("GET", `/fuzzy${queryString({ label, key, q: text, ...options })}`);
  }

  /** The match rules of the namespace. */
  matchRules(): Promise<MatchRule[]> {
    return this.#call("GET", "/match-rules");
  }

  /** Declares (or replaces) the match rule of a label. */
  setMatchRule(rule: MatchRule): Promise<MatchRule> {
    const { label, ...rest } = rule;
    return this.#call("PUT", `/match-rules/${encodeURIComponent(label)}`, rest);
  }

  async deleteMatchRule(label: string): Promise<void> {
    await this.#call("DELETE", `/match-rules/${encodeURIComponent(label)}`);
  }

  /** Nodes that may be this record, by the label's match rule (or `rule`), with reasons. */
  match(label: string, props: Record<string, unknown>, options: { neighbors?: Id[]; limit?: number; rule?: MatchRule } = {}): Promise<Match[]> {
    return this.#call("POST", "/match", { label, props, ...options });
  }

  /** Pairs of nodes of a label that its rule calls the same or to review. */
  matchPairs(label: string, limit?: number): Promise<{ a: Id; b: Id; match: Match }[]> {
    return this.#call("GET", `/matches${queryString({ label, limit })}`);
  }

  /** Reads new and changed texts and links what they mention; what it cannot decide goes to the review queue. */
  extract(options: { force?: boolean; source_labels?: string[]; entity_labels?: string[]; min_phrase_sources?: number; identifiers?: { label: string; pattern: string }[] } = {}): Promise<ExtractReport> {
    return this.#call("POST", "/extract", options);
  }

  /** What extraction could not decide: ambiguous names and new phrases. */
  reviewQueue(limit?: number): Promise<ReviewItem[]> {
    return this.#call("GET", `/review${queryString({ limit })}`);
  }

  /** Settles a review item; the decision is remembered for later runs. */
  resolveReview(key: string, resolution: { action: "link"; node: Id } | { action: "create"; label?: string } | { action: "dismiss" }): Promise<{ node: Id | null }> {
    return this.#call("POST", `/review/${encodeURIComponent(key)}`, resolution);
  }

  /** An agent's own mentions of a node's text, reconciled like extraction's. */
  addMentions(id: Id, mentions: { surface: string; label?: string; props?: Record<string, unknown>; start?: number; end?: number }[], defaultLabel?: string): Promise<ExtractReport> {
    return this.#call("POST", `/nodes/${encodeURIComponent(id)}/mentions`, { mentions, default_label: defaultLabel });
  }

  /** Records an access to each node now. */
  async touch(ids: Id[]): Promise<void> {
    await this.#call("POST", "/touch", { ids });
  }

  /** Memories whose strength fell below the threshold, weakest first. */
  weak(options: { half_life_days?: number; threshold?: number; label?: string; limit?: number } = {}): Promise<WeakMemory[]> {
    return this.#call("GET", `/weak${queryString(options)}`);
  }

  /** Adds the `Archived` label, which recall skips. Returns how many were archived. */
  async archive(ids: Id[]): Promise<number> {
    const result = await this.#call<{ archived: number }>("POST", "/archive", { ids });
    return result.archived;
  }

  /** Groups of older memories about one entity, ready to be summarized. */
  consolidation(
    options: { older_than_days?: number; min_size?: number; limit?: number; labels?: string[] } = {},
  ): Promise<ConsolidationGroup[]> {
    const { labels, ...rest } = options;
    return this.#call("GET", `/consolidation${queryString({ ...rest, labels: labels?.join(",") })}`);
  }

  /** Stores a summary of several memories and (by default) archives them. */
  summarize(input: SummaryInput): Promise<{ id: Id }> {
    return this.#call("POST", "/summaries", input);
  }
}

function queryString(values: Record<string, string | number | boolean | undefined>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined) params.set(key, String(value));
  }
  return params.size ? `?${params}` : "";
}
