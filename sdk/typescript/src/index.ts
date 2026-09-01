export interface MutinyDBOptions {
  baseUrl: string;
  tenant: string;
  operatorToken?: string;
  fetch?: typeof globalThis.fetch;
}

export interface SourceRef { system: string; record: string }
export interface WriteRequest {
  actor: string;
  session: string;
  branch: string;
  intent: string;
  sources: SourceRef[];
  table: string;
  rows: unknown[][];
}
export interface WriteReceipt { commit: number; epoch: number | null; rows: number }
export interface StandingAnswer { epoch: number; body: string }
export interface SemanticHit { rank: number; key: string; score: number }
export interface SemanticGroup { group_id: string; count: number; avg_cost: number; error_rate: number; exemplar_key: string; member_keys: string[] }

export class MutinyError extends Error {
  readonly retryable: boolean;
  readonly kind: string;
  constructor(readonly status: number, readonly body: string) {
    super(`MutinyDB ${status} ${body.trim()}`);
    this.name = "MutinyError";
    this.retryable = status === 429;
    this.kind = body.trim().split(/\s+/, 1)[0]?.replace(/:$/, "") ?? "Transport";
  }
}

export class MutinyDB {
  private readonly baseUrl: string;
  private readonly tenant: string;
  private readonly operatorToken: string | undefined;
  private readonly fetcher: typeof globalThis.fetch;

  constructor(options: MutinyDBOptions) {
    if (!options.tenant || options.tenant.includes("/") || options.tenant.includes("..")) {
      throw new TypeError("tenant must be a safe, non-empty identifier");
    }
    this.baseUrl = options.baseUrl.replace(/\/$/, "");
    this.tenant = options.tenant;
    this.operatorToken = options.operatorToken;
    this.fetcher = options.fetch ?? globalThis.fetch;
    if (!this.fetcher) throw new TypeError("a fetch implementation is required");
  }

  private async request(method: string, path: string, options: {params?: Record<string, string | number>; body?: unknown; text?: boolean; operator?: boolean} = {}): Promise<string> {
    // mutinyd v0.1 uses URI percent decoding but does not give `+` form-encoding semantics.
    const query = Object.entries(options.params ?? {}).map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`).join("&");
    const suffix = query ? `?${query}` : "";
    const headers: Record<string, string> = {Accept: "application/json, text/plain"};
    let body: string | undefined;
    if (options.body !== undefined) {
      body = options.text ? String(options.body) : JSON.stringify(options.body);
      headers["Content-Type"] = options.text ? "text/plain; charset=utf-8" : "application/json";
    }
    if (options.operator) {
      if (!this.operatorToken) throw new TypeError("operatorToken is required for this operation");
      headers.Authorization = `Bearer ${this.operatorToken}`;
    }
    const init: RequestInit = {method, headers};
    if (body !== undefined) init.body = body;
    const response = await this.fetcher(`${this.baseUrl}/v1/${encodeURIComponent(this.tenant)}/${path}${suffix}`, init);
    const responseBody = await response.text();
    if (!response.ok) throw new MutinyError(response.status, responseBody);
    return responseBody;
  }

  health(): Promise<string> { return this.request("GET", "health"); }

  async write(write: WriteRequest): Promise<WriteReceipt> {
    return JSON.parse(await this.request("POST", "write", {params: {format: "json"}, body: write})) as WriteReceipt;
  }

  async register(sql: string, unboundedReason?: string): Promise<number> {
    const options: {params?: Record<string, string | number>; body: string; text: true} = {body: sql, text: true};
    if (unboundedReason) options.params = {unbounded: unboundedReason};
    return Number((await this.request("POST", "sql/register", options)).trim());
  }

  async deregister(handle: number): Promise<void> { await this.request("POST", "sql/deregister", {params: {handle}}); }

  async read(handle: number): Promise<StandingAnswer> {
    const answer = JSON.parse(await this.request("GET", "sql/read", {params: {handle, format: "json"}}));
    return {epoch: answer.epoch, body: answer.answer};
  }

  oneshot(sql: string): Promise<string> { return this.request("POST", "query/oneshot", {body: {sql}}); }

  async subscribe(handle: number, from = 0): Promise<{token: number; deltas: StandingAnswer[]}> {
    const subscription = JSON.parse(await this.request("GET", "sql/subscribe", {params: {handle, from, format: "json"}}));
    return {token: subscription.token, deltas: subscription.deltas.map((delta: {epoch: number; answer: string}) => ({epoch: delta.epoch, body: delta.answer}))};
  }

  plan(handle: number): Promise<string> { return this.request("GET", "sql/plan", {params: {handle}}); }

  sessionOpen(session: string): Promise<{session: string; branch: string; token: unknown}> {
    return this.request("POST", "session/open", {body: {session}}).then(JSON.parse);
  }

  async fork(session: string, from: string, child: string): Promise<void> { await this.request("POST", "branch/fork", {body: {session, from, child}}); }
  async merge(session: string, child: string, into: string): Promise<number> { return Number((await this.request("POST", "branch/merge", {body: {session, child, into}})).trim().replace("merged ", "")); }
  async rewind(session: string, child: string): Promise<number> { return Number((await this.request("POST", "branch/rewind", {body: {session, child}})).trim().replace("freed ", "")); }

  async semanticAnswer(branch: string, query: string): Promise<SemanticHit[]> {
    return JSON.parse(await this.request("GET", "semantic/answer", {params: {branch, query, format: "json"}})) as SemanticHit[];
  }

  async semanticGroups(branch: string, group: string): Promise<SemanticGroup[]> {
    return JSON.parse(await this.request("GET", "semantic/groups", {params: {branch, group, format: "json"}})) as SemanticGroup[];
  }

  async propose(input: {actor: string; branch: string; actionType: string; target: string; idempotencyKey: string; justifiedBy?: string[]}): Promise<string> {
    const body = {actor: input.actor, branch: input.branch, action_type: input.actionType, target: input.target, idempotency_key: input.idempotencyKey, justified_by: input.justifiedBy ?? []};
    return (await this.request("POST", "action/propose", {body})).trim().replace("proposed ", "");
  }

  execute(proposal: string): Promise<string> { return this.request("POST", "action/execute", {body: {proposal}, operator: true}); }
  taint(system: string, record: string): Promise<string> { return this.request("POST", "taint", {body: {system, record}, operator: true}); }

  async mcpCall<T = unknown>(name: string, args: Record<string, unknown>, id: string | number = 1): Promise<T> {
    const response = JSON.parse(await this.request("POST", "mcp", {body: {jsonrpc: "2.0", id, method: "tools/call", params: {name, arguments: args}}}));
    if (response.error) throw new MutinyError(200, response.error.message ?? "MCP error");
    return response.result.structured as T;
  }
}
