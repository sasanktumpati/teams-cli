import {
  array,
  bearer,
  boolean,
  defineApp,
  defineProvider,
  json,
  mutation,
  number,
  object,
  query,
  router,
  secrets,
  string,
} from "apps";
import type { AccountRequest, Infer } from "apps";

const DEFAULT_PORT = 47613;
function configuredPort(): number {
  const raw = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env
    ?.TEAMS_CLI_PORT;
  const n = raw !== undefined && /^\s*\d+\s*$/.test(raw) ? Number(raw) : NaN;
  return Number.isInteger(n) && n > 0 && n < 65536 ? n : DEFAULT_PORT;
}
const PORT = configuredPort();
const BASE = `http://127.0.0.1:${PORT}`;

const teamsArchive = defineProvider({
  name: "Teams Archive",
  hosts: [`127.0.0.1:${PORT}`],
  auth: {
    token: secrets({
      label: "API token",
      guide: "Run `teams token create executor` on your Mac and paste the token below.",
      fields: object({
        token: string({ description: "Bearer token created by `teams token create executor`" }),
      }),
      request: ({ token }) => bearer(token),
    }),
  },
  health: async ({ account, fetch }) => {
    const res = await fetch(`${BASE}/v1/status`, { headers: account.headers(), cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
  },
});

const When = (what: string) =>
  string({
    description: `${what}. Relative (30m, 12h, 7d, 2w), today, yesterday, or ISO (2026-11-01, 2026-11-01T14:00); "all" (or an empty string) clears a default window (e.g. the 7d default) to cover all time.`,
  }).optional();

const Chat = object({
  id: string(),
  name: string().optional(),
  kind: string({ description: "chat | channel | meeting | team | community | system" }),
  topic: string().optional(),
  teamId: string().optional(),
  messageCount: number(),
  lastActivity: string({ description: "ISO 8601 with offset" }).optional(),
  lastActivityMs: number().optional(),
});

const Reaction = object({ emoji: string(), count: number(), users: array(string()) });
const Attachment = object({
  kind: string({ description: "file | link | image" }),
  name: string().optional(),
  url: string(),
});

const MessageRevision = object({
  text: string(),
  editedAt: string().optional(),
  capturedAt: string({ description: "When the archive captured this version" }).optional(),
});

const Message = object({
  id: string(),
  chatId: string(),
  threadId: string().optional(),
  isReply: boolean({ description: "True for a reply inside a channel thread" }),
  time: string({ description: "ISO 8601 with offset" }).optional(),
  timeMs: number().optional(),
  sender: string().optional(),
  senderId: string().optional(),
  kind: string({ description: "message | event (joins, renames, calls, recordings)" }),
  text: string({ description: "Plain text. Quoted replies are lines starting with '> '." }),
  subject: string().optional(),
  importance: string().optional(),
  edited: boolean(),
  deleted: boolean({ description: "Deleted in Teams; text is the last version archived, if any" }),
  reactions: array(Reaction),
  attachments: array(Attachment),
  chatName: string().optional(),
  chatKind: string().optional(),
  snippet: string({ description: "Search only: matching excerpt, matches wrapped in « »" }).optional(),
  editedAt: string({ description: "When last edited (ISO 8601)" }).optional(),
  deletedAt: string({ description: "When deleted in Teams (ISO 8601)" }).optional(),
  deletedBy: string({ description: "Who deleted it, when known" }).optional(),
  history: array(MessageRevision).optional(),
  matchedInHistory: boolean({
    description: "Search with includeHistory: only an earlier version matched (the words were edited out)",
  }).optional(),
});

const OtherMatch = object({ id: string(), name: string().optional(), kind: string() });
const MemberMatch = object({ id: string(), name: string().optional(), email: string().optional() });
const SenderMatch = object({
  id: string().optional(),
  name: string().optional(),
  email: string().optional(),
  messages: number({ description: "Messages this sender has in the archive" }),
});

const ChatsOutput = object({
  chats: array(Chat),
  total: number(),
  memberMatches: array(MemberMatch).optional(),
});
const MessagesOutput = object({
  chat: Chat,
  messages: array(Message),
  matched: number({ description: "Messages matching the filters (before limit)" }),
  truncated: boolean({ description: "True when limit (or context) cut off matches" }),
  otherMatches: array(OtherMatch),
  aroundMessageId: string({ description: "With around: the anchor message's id" }).optional(),
  hasBefore: boolean({ description: "With around: more messages exist before the window" }).optional(),
  hasAfter: boolean({ description: "With around: more messages exist after the window" }).optional(),
});
const SearchOutput = object({
  query: string().optional(),
  chat: Chat.optional(),
  senders: array(SenderMatch).optional(),
  results: array(Message).optional(),
  matched: number(),
  truncated: boolean().optional(),
  countOnly: boolean().optional(),
});

const SharedAttachment = object({
  kind: string({ description: "file | image | link" }),
  name: string().optional(),
  type: string({ description: "File extension, e.g. pdf" }).optional(),
  url: string(),
  time: string().optional(),
  timeMs: number().optional(),
  sender: string().optional(),
  senderId: string().optional(),
  chatId: string(),
  chatName: string().optional(),
  chatKind: string().optional(),
  messageId: string(),
  threadId: string().optional(),
  messageDeleted: boolean(),
  messageText: string({ description: "First 200 characters of the message" }),
  localPath: string({ description: "Local OneDrive path if synced in Microsoft Teams Chat Files" }).optional(),
});
const AttachmentsOutput = object({
  chat: Chat.optional(),
  senders: array(SenderMatch).optional(),
  attachments: array(SharedAttachment),
  matched: number(),
  truncated: boolean(),
});

const ChangesOutput = object({
  chat: Chat.optional(),
  changes: array(Message),
  matched: number(),
  truncated: boolean(),
});

const RecordSource = object({
  source: string(),
  records: number(),
  versions: number(),
  lastCaptured: string().optional(),
});
const RawRecord = object({
  key: string(),
  capturedAt: string().optional(),
  data: json().optional(),
});
const RecordsOutput = object({
  sources: array(RecordSource).optional(),
  source: string().optional(),
  records: array(RawRecord).optional(),
  matched: number().optional(),
  truncated: boolean().optional(),
});

const Person = object({
  id: string(),
  name: string().optional(),
  email: string().optional(),
  jobTitle: string().optional(),
  department: string().optional(),
  kind: string().optional(),
  messagesInArchive: number(),
});
const PeopleOutput = object({ people: array(Person) });

const PersonOutput = object({
  person: object({
    id: string(),
    name: string().optional(),
    email: string().optional(),
    jobTitle: string().optional(),
    department: string().optional(),
    kind: string().optional(),
    messagesInArchive: number(),
    firstContact: string().optional(),
    lastContact: string().optional(),
  }),
  sharedChats: array(
    object({
      id: string(),
      name: string().optional(),
      kind: string(),
      messagesFromPerson: number(),
      messageCount: number(),
      lastActivity: string().optional(),
      lastActivityMs: number().optional(),
    }),
  ),
  totalSharedChats: number(),
  recentFiles: array(
    object({
      kind: string(),
      name: string().optional(),
      type: string().optional(),
      url: string(),
      time: string().optional(),
      timeMs: number().optional(),
      chatId: string(),
      chatName: string().optional(),
      messageId: string(),
    }),
  ),
  totalAttachments: number(),
  otherMatches: array(
    object({ id: string(), name: string().optional(), email: string().optional(), messagesInArchive: number() }),
  ),
});

const Mention = object({
  messageId: string(),
  chatId: string(),
  chatName: string().optional(),
  chatKind: string().optional(),
  threadId: string().optional(),
  time: string().optional(),
  timeMs: number().optional(),
  sender: string(),
  senderId: string().optional(),
  text: string(),
  isRead: boolean({ description: "False when unread in Teams; absent when unknown" }).optional(),
});
const MentionsOutput = object({
  me: string({ description: "Your Teams id (MRI), when known" }).optional(),
  chat: Chat.optional(),
  mentions: array(Mention),
  matched: number(),
  truncated: boolean(),
});

const InboxOutput = object({
  me: object({ id: string(), name: string().optional() }).optional(),
  since: string().optional(),
  unreadChats: array(
    object({
      chatId: string(),
      chatName: string().optional(),
      kind: string(),
      unreadCount: number(),
      lastMessageId: string().optional(),
      lastMessageAt: string().optional(),
      lastMessageMs: number().optional(),
      lastSender: string().optional(),
      lastText: string(),
      readUpTo: string().optional(),
    }),
  ),
  awaitingReply: array(
    object({
      chatId: string(),
      chatName: string().optional(),
      kind: string(),
      lastMessageId: string(),
      lastMessageAt: string().optional(),
      lastMessageMs: number().optional(),
      lastSender: string().optional(),
      lastSenderId: string().optional(),
      lastText: string(),
      unread: boolean(),
    }),
  ),
  mentions: array(Mention),
  unreadActivities: array(
    object({
      id: string(),
      activityType: string().optional(),
      activitySubtype: string().optional(),
      chatId: string().optional(),
      chatName: string().optional(),
      messageId: string().optional(),
      sender: string().optional(),
      preview: string().optional(),
      time: string().optional(),
      timeMs: number().optional(),
    }),
  ),
  counts: object({
    unreadChats: number(),
    awaitingReply: number(),
    mentions: number(),
    unreadActivities: number(),
  }),
});

const Participant = object({ id: string().optional(), name: string().optional() });
const CallsOutput = object({
  calls: array(
    object({
      id: string(),
      time: string().optional(),
      timeMs: number().optional(),
      endTime: string().optional(),
      durationSeconds: number().optional(),
      durationMs: number().optional(),
      direction: string().optional(),
      callType: string().optional(),
      state: string({ description: "e.g. accepted, missed, declined" }).optional(),
      originator: Participant.optional(),
      target: Participant.optional(),
      participants: array(Participant),
      chatId: string().optional(),
      chatName: string().optional(),
    }),
  ),
  matched: number(),
  truncated: boolean(),
});

const MeetingsOutput = object({
  meetings: array(
    object({
      id: string(),
      subject: string().optional(),
      start: string().optional(),
      startMs: number().optional(),
      end: string().optional(),
      endMs: number().optional(),
      organizer: string().optional(),
      organizerEmail: string().optional(),
      location: string().optional(),
      isOnlineMeeting: boolean(),
      joinUrl: string().optional(),
      myResponse: string().optional(),
      showAs: string().optional(),
      chatId: string({ description: "Meeting chat id; read it with messages({chat})" }).optional(),
      attendees: array(
        object({
          name: string().optional(),
          email: string().optional(),
          response: string().optional(),
          type: string().optional(),
        }),
      ),
      preview: string().optional(),
    }),
  ),
  matched: number(),
  truncated: boolean(),
});

const SavedOutput = object({
  saved: array(
    object({
      messageId: string(),
      chatId: string(),
      chatName: string().optional(),
      chatKind: string().optional(),
      threadId: string().optional(),
      savedAt: string().optional(),
      savedAtMs: number().optional(),
      time: string().optional(),
      timeMs: number().optional(),
      sender: string().optional(),
      senderId: string().optional(),
      text: string(),
      deleted: boolean(),
      unsaved: boolean(),
    }),
  ),
  matched: number(),
  truncated: boolean(),
});

const PinnedOutput = object({
  chat: Chat.optional(),
  pinned: array(
    object({
      messageId: string(),
      chatId: string(),
      chatName: string().optional(),
      chatKind: string().optional(),
      threadId: string().optional(),
      time: string().optional(),
      timeMs: number().optional(),
      sender: string().optional(),
      senderId: string().optional(),
      subject: string().optional(),
      text: string(),
      source: string(),
    }),
  ),
  matched: number(),
  truncated: boolean(),
});

const SyncRun = object({
  status: string(),
  startedAt: string().optional(),
  finishedAt: string().optional(),
  message: string().optional(),
  stats: json().optional(),
});
const StatusOutput = object({
  healthy: boolean(),
  problems: array(string()),
  archive: object({
    path: string(),
    sizeBytes: number(),
    schemaVersion: number(),
    diskFreeBytes: number(),
  }),
  counts: object({
    messages: number(),
    events: number(),
    chats: number(),
    people: number(),
    revisions: number(),
    attachments: number(),
    recordVersions: number({ description: "Raw Teams store records kept (every version)" }),
    preservedAfterTeamsDropped: number({ description: "Messages kept that Teams no longer caches" }),
  }),
  range: object({ oldest: string().optional(), newest: string().optional() }),
  lastChecked: string().optional(),
  lastSync: SyncRun.optional(),
  lastSuccessfulSync: SyncRun.optional(),
  backups: object({ count: number(), latest: string({ description: "Path of the newest backup" }).optional() }),
});

type Fetch = (input: string, init?: RequestInit) => Promise<Response>;
type Ctx = { readonly fetch: Fetch; readonly app: { readonly accounts: { readonly archive: AccountRequest } } };

function dropNulls(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(dropNulls);
  if (v && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, x] of Object.entries(v)) if (x !== null) out[k] = dropNulls(x);
    return out;
  }
  return v;
}

function dropNullsKeepRawData(v: unknown): unknown {
  if (!v || typeof v !== "object" || Array.isArray(v)) return dropNulls(v);
  const { records, ...rest } = v as Record<string, unknown>;
  const out = dropNulls(rest) as Record<string, unknown>;
  if (Array.isArray(records)) {
    out.records = records.map((rec) => {
      if (!rec || typeof rec !== "object" || Array.isArray(rec)) return dropNulls(rec);
      const { data, ...meta } = rec as Record<string, unknown>;
      const r = dropNulls(meta) as Record<string, unknown>;
      if (data !== null && data !== undefined) r.data = data;
      return r;
    });
  }
  return out;
}

async function call(
  ctx: Ctx,
  path: string,
  params: Record<string, string | number | boolean | undefined> = {},
  method: "GET" | "POST" = "GET",
  opts: { preserveRawData?: boolean } = {},
): Promise<unknown> {
  const url = new URL(path, BASE);
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined) continue;
    if (v === "" && k !== "since") continue;
    url.searchParams.set(k, String(v));
  }
  const headers = ctx.app.accounts.archive.headers();
  let res: Response;
  try {
    res = await ctx.fetch(url.toString(), { method, cache: "no-store", headers });
  } catch (e) {
    throw new Error(
      "The local Teams archive isn't reachable at " + BASE + ". On the Mac, run `teams daemon install` " +
        "(or `teams status` to diagnose). " + String(e),
    );
  }
  const body = (await res.json().catch(() => ({}))) as {
    error?: { code?: string; message?: string; details?: unknown };
  };
  if (!res.ok || body.error) {
    const details = body.error?.details !== undefined ? ` (${JSON.stringify(body.error.details)})` : "";
    const hint =
      res.status === 401
        ? " Create a new token with `teams token create executor` and reconnect the Teams Archive account."
        : "";
    throw new Error(
      `teams ${body.error?.code ?? res.status}: ${body.error?.message ?? res.statusText}${details}${hint}`,
    );
  }
  return opts.preserveRawData ? dropNullsKeepRawData(body) : dropNulls(body);
}

const instructions = `Local archive of the user's Microsoft Teams chats, channels and meetings, synced from the
Teams desktop cache automatically (checked every minute). It keeps messages forever, even after Teams drops them, and keeps
every earlier version of edited or deleted messages. Read-only, except \`sync\`.

- What needs attention: \`inbox()\` (unread chats, chats awaiting your reply, mentions, unread activity);
  \`mentions({unreadOnly})\` for @mentions of you.
- Find a chat with \`chats({query})\`, or every chat with someone via \`chats({member})\`; read it with
  \`messages({chat})\`. \`chat\` accepts an id or a name fragment; ambiguous names use the best match and
  list the rest in otherMatches.
- \`messages\` returns the newest \`limit\` matches in chronological order (limit 0 = all). Use \`since\`
  to bound long chats and check \`truncated\`. \`messages({around: id, context})\` shows the conversation
  around one message (e.g. a search hit; pass "chatId/msgId" or just the id). includeHistory adds earlier versions.
- \`search\` is full-text: every word must match (as a prefix), "quoted phrases" match exactly, OR / NOT
  and -exclude / -"exclude phrase" work. Narrow with chat, sender, since, until, hasAttachments. Omit query
  to list everything matching the filters, e.g. search({sender: "Ana", since: "7d"}). includeHistory also
  matches text since edited out; countOnly: true returns just the number. Newest first unless sort: "relevance".
- \`person({person})\`: profile, first/last contact, shared chats and files. \`people\` lists everyone.
- \`attachments\`: files, images and links shared (filter kind, type: "pdf", sender, chat).
- \`changes({since, what})\`: messages edited or deleted, with their earlier text in history.
- \`calls\` (missedOnly, person), \`meetings\` (calendar; chatId reads the meeting chat), \`saved\`, \`pinned\`.
- \`records()\` lists raw Teams stores; \`records({source})\` returns their raw JSON. Use only when no typed tool fits.
- Text is plain: "> Name: ..." lines are quoted replies, @Name is a mention, [file: x] / [image] mark
  attachments (URLs are in attachments).
- In code mode, filter and summarize inside the program and return only what you need.
- If a call fails with "isn't reachable", the archive daemon on the Mac is down; say so rather than retrying.`;

const limit = (def: string) => number({ description: `Max results (default ${def}, 0 = all)` }).optional();

export default defineApp({ app: { accounts: { archive: teamsArchive } } }, {
  tools: router(
    {
      status: query(
        {
          description: "Archive health: counts, size, date range, last sync, backups, and any problems.",
          input: object({}),
          output: StatusOutput,
        },
        async (ctx) => StatusOutput.parse(await call(ctx, "/v1/status")),
      ),

      inbox: query(
        {
          description:
            "What may need your attention: unread chats, chats whose last message is from someone else, mentions and unread activity.",
          input: object({
            since: When('Only activity at or after this time (default 7d; "all" for all time)'),
            limit: limit("30"),
          }),
          output: InboxOutput,
        },
        async (ctx, input) => InboxOutput.parse(await call(ctx, "/v1/inbox", input)),
      ),

      mentions: query(
        {
          description: "Messages that @mention you, newest first (includes old mentions Teams no longer shows).",
          input: object({
            since: When("Only mentions at or after this time"),
            until: When("Only mentions before this time"),
            chat: string({ description: "Limit to one chat (id or name fragment)" }).optional(),
            unreadOnly: boolean({ description: "Only mentions Teams marks unread" }).optional(),
            limit: limit("50"),
          }),
          output: MentionsOutput,
        },
        async (ctx, input) => MentionsOutput.parse(await call(ctx, "/v1/mentions", input)),
      ),

      chats: query(
        {
          description: "List chats, channels and meetings, most recently active first. Filter by name or member.",
          input: object({
            query: string({ description: "Name fragment, e.g. 'project alpha'" }).optional(),
            kind: string({ description: "chat | channel | meeting | team | community | system" }).optional(),
            member: string({
              description: "Filter to chats this person is in or has posted in (name, email or id)",
            }).optional(),
            limit: limit("50"),
            includeSystem: boolean({ description: "Include system feeds (notifications, call logs)" }).optional(),
          }),
          output: ChatsOutput,
        },
        async (ctx, input) => ChatsOutput.parse(await call(ctx, "/v1/chats", input)),
      ),

      messages: query(
        {
          description:
            "Read one chat's messages in chronological order (newest `limit` that match), or the context around one message.",
          input: object({
            chat: string({ description: "Chat id, or a fragment of its name (optional when `around` is given)" }).optional(),
            around: string({ description: "Message id or chatId/msgId to read surrounding context" }).optional(),
            context: number({ description: "Messages before and after `around`, default 10" }).optional(),
            since: When("Only messages at or after this time"),
            until: When("Only messages before this time (a bare date includes that whole day)"),
            sender: string({ description: "Only messages from this person (name or email fragment)" }).optional(),
            limit: number({ description: "Max messages (default 200, 0 = all)" }).optional(),
            includeEvents: boolean({ description: "Include joins, renames, calls, recordings" }).optional(),
            thread: string({ description: "Channel thread root message id" }).optional(),
            includeHistory: boolean({ description: "Include earlier versions of edited messages" }).optional(),
          }),
          output: MessagesOutput,
        },
        async (ctx, input) => {
          if (!input.chat && !input.around) throw new Error("messages needs `chat` or `around`.");
          return MessagesOutput.parse(await call(ctx, "/v1/messages", input));
        },
      ),

      search: query(
        {
          description:
            "Full-text search across every archived message, newest first. Omit query to list all messages matching the filters.",
          input: object({
            query: string({
              description: 'Words (all must match, as prefixes), "exact phrase", OR, NOT, -exclude. Optional with filters.',
            }).optional(),
            chat: string({ description: "Limit to one chat (id or name fragment)" }).optional(),
            sender: string({ description: "Limit to one sender (name or email fragment)" }).optional(),
            since: When("Only messages at or after this time"),
            until: When("Only messages before this time"),
            limit: limit("30"),
            sort: string({ description: "recent (default) | relevance" }).optional(),
            includeEvents: boolean().optional(),
            includeHistory: boolean({ description: "Also search earlier versions of edited messages" }).optional(),
            countOnly: boolean({ description: "Return only the match count, no message bodies" }).optional(),
            hasAttachments: boolean({ description: "Only messages with at least one file, image or link" }).optional(),
          }),
          output: SearchOutput,
        },
        async (ctx, input) => {
          if (
            !input.query?.trim() &&
            !input.sender &&
            !input.chat &&
            !input.since &&
            !input.until &&
            !input.hasAttachments
          ) {
            throw new Error("search needs a query, or a filter such as sender, chat, since, until or hasAttachments.");
          }
          return SearchOutput.parse(await call(ctx, "/v1/search", input));
        },
      ),

      attachments: query(
        {
          description: "Files, images and links shared in messages, newest first, with who shared them where.",
          input: object({
            query: string({ description: "Name or URL fragment" }).optional(),
            chat: string({ description: "Limit to one chat (id or name fragment)" }).optional(),
            sender: string({ description: "Limit to one sender (name or email fragment)" }).optional(),
            kind: string({ description: "file | image | link" }).optional(),
            type: string({ description: "File extension, e.g. pdf, xlsx" }).optional(),
            since: When("Only shared at or after this time"),
            until: When("Only shared before this time"),
            limit: limit("50"),
          }),
          output: AttachmentsOutput,
        },
        async (ctx, input) => AttachmentsOutput.parse(await call(ctx, "/v1/attachments", input)),
      ),

      changes: query(
        {
          description: "Messages edited or deleted in a time window, most recent change first, with earlier versions in history.",
          input: object({
            since: When('Changes at or after this time (default 7d; "all" for all time)'),
            until: When("Changes before this time"),
            chat: string({ description: "Limit to one chat (id or name fragment)" }).optional(),
            what: string({ description: "all | edited | deleted" }).optional(),
            limit: limit("100"),
          }),
          output: ChangesOutput,
        },
        async (ctx, input) => ChangesOutput.parse(await call(ctx, "/v1/changes", input)),
      ),

      people: query(
        {
          description: "People seen in Teams, with email and job title, most active first.",
          input: object({
            query: string({ description: "Name, email or title fragment" }).optional(),
            limit: limit("50"),
            includeBots: boolean().optional(),
          }),
          output: PeopleOutput,
        },
        async (ctx, input) => PeopleOutput.parse(await call(ctx, "/v1/people", input)),
      ),

      person: query(
        {
          description: "One person's profile: messages, first/last contact, chats you share, and files they shared.",
          input: object({
            person: string({ description: "Name fragment, email or id", minLength: 1 }),
            limit: number({ description: "Max shared chats and files (default 20, 0 = all)" }).optional(),
          }),
          output: PersonOutput,
        },
        async (ctx, input) => PersonOutput.parse(await call(ctx, "/v1/person", input)),
      ),

      calls: query(
        {
          description: "Call history, newest first, with participants, duration and state (e.g. missed).",
          input: object({
            since: When("Only calls at or after this time"),
            until: When("Only calls before this time"),
            person: string({ description: "Only calls with this person (name, email or id fragment)" }).optional(),
            missedOnly: boolean().optional(),
            limit: limit("50"),
          }),
          output: CallsOutput,
        },
        async (ctx, input) => CallsOutput.parse(await call(ctx, "/v1/calls", input)),
      ),

      meetings: query(
        {
          description: "Calendar events Teams cached, newest start first, with attendees and join links.",
          input: object({
            since: When("Only meetings ending at or after this time"),
            until: When("Only meetings starting before this time"),
            query: string({ description: "Subject, organizer, location or attendee fragment" }).optional(),
            limit: limit("50"),
          }),
          output: MeetingsOutput,
        },
        async (ctx, input) => MeetingsOutput.parse(await call(ctx, "/v1/meetings", input)),
      ),

      saved: query(
        {
          description: "Messages you saved (bookmarked) in Teams, most recently saved first.",
          input: object({
            includeUnsaved: boolean({ description: "Include messages you later unsaved" }).optional(),
            limit: limit("50"),
          }),
          output: SavedOutput,
        },
        async (ctx, input) => SavedOutput.parse(await call(ctx, "/v1/saved", input)),
      ),

      pinned: query(
        {
          description: "Messages pinned in chats and channels, newest first.",
          input: object({
            chat: string({ description: "Limit to one chat (id or name fragment)" }).optional(),
            limit: limit("50"),
          }),
          output: PinnedOutput,
        },
        async (ctx, input) => PinnedOutput.parse(await call(ctx, "/v1/pinned", input)),
      ),

      records: query(
        {
          description:
            "Raw Teams store records exactly as cached. Omit source to list the available sources; prefer typed tools.",
          input: object({
            source: string({ description: "Source name from records() (omit to list sources)" }).optional(),
            key: string({ description: "One record key" }).optional(),
            since: When("Only versions captured at or after this time"),
            allVersions: boolean({ description: "Every captured version, not just the latest per record" }).optional(),
            limit: limit("50"),
          }),
          output: RecordsOutput,
        },
        async (ctx, input) => RecordsOutput.parse(await call(ctx, "/v1/records", input, "GET", { preserveRawData: true })),
      ),

      sync: mutation(
        {
          description: "Pull the latest messages from Teams into the archive now (normally synced automatically every minute on change).",
          input: object({}),
          output: object({ ok: boolean() }),
        },
        async (ctx) => {
          await call(ctx, "/v1/sync", {}, "POST");
          return { ok: true };
        },
      ),
    },
    {
      title: "Teams (local archive)",
      description: "Search and read your Microsoft Teams chats, mentions, calls and meetings from a durable archive on this Mac.",
      instructions,
    },
  ),
});

export type MessageT = Infer<typeof Message>;
