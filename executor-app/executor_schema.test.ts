import { expect, mock, test } from "bun:test";

const appIndex = new URL("./index.ts", import.meta.url).pathname;
const realApps = await import(Bun.resolveSync("apps", appIndex));
const origQuery = (realApps as any).query;
const origMutation = (realApps as any).mutation;
const origRouter = (realApps as any).router;

const TARGET = process.env.TEAMS_URL ?? process.env.TEAMS_RS_URL;
const TOKEN = process.env.TEAMS_TOKEN ?? "";
const BASE = "http://127.0.0.1:47613";

type ToolCtx = {
  fetch: typeof fetch;
  app: { accounts: { archive: { headers(): Record<string, string>; url(u: string | URL): string } } };
};
type Handler = (ctx: ToolCtx, input: Record<string, unknown>) => Promise<unknown>;
const handlers = new WeakMap<object, Handler>();
const tools: Record<string, Handler> = {};

mock.module("apps", () => ({
  ...realApps,
  query: (def: unknown, handler: Handler) => {
    const q = origQuery(def, handler);
    handlers.set(q, handler);
    return q;
  },
  mutation: (def: unknown, handler: Handler) => {
    const m = origMutation(def, handler);
    handlers.set(m, handler);
    return m;
  },
  router: (defs: Record<string, object>, meta: unknown) => {
    for (const [name, d] of Object.entries(defs)) {
      const h = handlers.get(d);
      if (h) tools[name] = h;
    }
    return origRouter(defs, meta);
  },
}));

await import("./index.ts");

const via = (target: string): typeof fetch =>
  ((input: string | URL | Request, init?: RequestInit) =>
    fetch(String(input).replace(BASE, target), init)) as typeof fetch;

const account = (token: string) => ({
  headers: () => ({ authorization: `Bearer ${token}` }),
  url: (u: string | URL) => String(u),
});

const seenAuth: (string | null)[] = [];
const recording = (inner: typeof fetch): typeof fetch =>
  ((input: string | URL | Request, init?: RequestInit) => {
    seenAuth.push(new Headers(init?.headers).get("authorization"));
    return inner(input, init);
  }) as typeof fetch;

async function outcome(target: string, name: string, input: Record<string, unknown>, token = TOKEN) {
  try {
    const ctx: ToolCtx = { fetch: recording(via(target)), app: { accounts: { archive: account(token) } } };
    return { ok: await tools[name](ctx, input) };
  } catch (e) {
    return { error: String(e) };
  }
}

const HAPPY_CASES: [string, Record<string, unknown>][] = [
  ["status", {}],
  ["inbox", {}],
  ["inbox", { since: "30d", limit: 1 }],
  ["inbox", { since: "all" }],
  ["mentions", {}],
  ["mentions", { unreadOnly: true }],
  ["chats", {}],
  ["chats", { query: "launch" }],
  ["chats", { member: "bob@corp.test" }],
  ["chats", { limit: 0 }],
  ["messages", { chat: "Launch crew" }],
  ["messages", { chat: "Launch", includeHistory: true, includeEvents: true }],
  ["messages", { around: "1795000000003", context: 1 }],
  ["messages", { chat: "Bob Builder" }],
  ["search", { query: "group" }],
  ["search", { query: "budget", includeHistory: true }],
  ["search", { query: "budget", includeHistory: true, countOnly: true }],
  ["search", { sender: "Bob" }],
  ["search", { query: "note", sort: "relevance", limit: 2 }],
  ["attachments", {}],
  ["attachments", { kind: "file" }],
  ["changes", {}],
  ["changes", { since: "all" }],
  ["people", {}],
  ["people", { query: "ada" }],
  ["person", { person: "Ada Lovelace" }],
  ["calls", {}],
  ["calls", { missedOnly: true, person: "bob" }],
  ["meetings", {}],
  ["meetings", { query: "sprint" }],
  ["saved", {}],
  ["saved", { includeUnsaved: true }],
  ["pinned", {}],
  ["pinned", { chat: "Launch" }],
  ["records", {}],
  ["records", { source: "call-history-manager/call-history" }],
  ["records", { source: "call-history-manager/call-history", key: "c2", allVersions: true }],
  ["sync", {}],
];

const ERROR_CASES: [string, Record<string, unknown>, string][] = [
  ["messages", { chat: "no-such-chat" }, "chat_not_found"],
  ["messages", {}, "messages needs `chat` or `around`"],
  ["search", {}, "search needs a query"],
  ["person", { person: "nobody-here" }, "person_not_found"],
];

test.skipIf(!TARGET || !TOKEN)("every Executor tool parses the Rust daemon cleanly", async () => {
  const covered = new Set(HAPPY_CASES.map(([n]) => n));
  expect(Object.keys(tools).sort()).toEqual([...covered].sort());

  for (const [name, input] of HAPPY_CASES) {
    const r = (await outcome(TARGET!, name, input)) as { ok?: unknown; error?: string };
    expect({ name, input, error: r.error }).toEqual({ name, input, error: undefined });
    expect(r.ok).toBeDefined();
  }

  for (const [name, input, needle] of ERROR_CASES) {
    const r = (await outcome(TARGET!, name, input)) as { error?: string };
    expect(r.error).toBeDefined();
    expect(r.error).toContain(needle);
  }

  expect(seenAuth.length).toBeGreaterThanOrEqual(HAPPY_CASES.length);
  expect(new Set(seenAuth)).toEqual(new Set([`Bearer ${TOKEN}`]));

  for (const name of covered) {
    const input = HAPPY_CASES.find(([n]) => n === name)![1];
    const r = (await outcome(TARGET!, name, input, "not-the-token")) as { error?: string };
    expect({ name, error: r.error }).toEqual({ name, error: expect.stringContaining("teams unauthorized") });
    expect(r.error).toContain("teams token");
  }
});
