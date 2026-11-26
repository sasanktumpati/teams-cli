# teams-cli

Local-first archive, CLI, and scoped loopback HTTP API for Microsoft Teams (v2) on macOS.

Reads the Teams desktop IndexedDB/LevelDB cache into an append-only SQLite archive (`~/Library/Application Support/teams-cli/archive.sqlite3`) so messages, edits, deletions, attachments, calls, and meetings are kept permanently even after Teams evicts them — and supports live Teams actions (sending messages with Markdown, `@mentions`, quotes, inline images, and OneDrive/SharePoint file attachments; reactions, edits, deletes, pins, saves, forwards, chat management, and live presence) protected by explicit guardrails and scoped bearer tokens.

## Install

Requires macOS with Microsoft Teams (v2).

```sh
curl -fsSL https://raw.githubusercontent.com/sasanktumpati/teams-cli/master/install.sh | sh
```

Or build from source with Rust 1.89+:

```sh
cargo install --path . --force
```

Then set up the archive and background daemon:

```sh
teams sync
teams daemon install
teams doctor
```

To allow scoped live write routes on the local HTTP API, opt in explicitly:

```sh
teams daemon install --allow-writes
```

### Shell Completions

```sh
teams completions zsh  > "${fpath[1]}/_teams"
teams completions fish > ~/.config/fish/completions/teams.fish
teams completions bash > ~/.local/share/bash-completion/completions/teams
```

## Usage

Run `teams docs` for the full command, scope, flag, and HTTP API reference.

```sh
teams status
teams doctor
teams sync

teams inbox
teams mentions --unread
teams chat list --grep "project"
teams chat list --member alice@example.com
teams msg list "Project" --since 7d
teams msg list --around 1795219003436 --context 10
teams msg saved
teams msg pinned

teams search "launch plan" -draft --from Bob --since 30d
teams search '"exact phrase" OR other' --relevance
teams search budget --history
teams search --has-attachments --since 30d

teams people --grep alice
teams people alice@example.com
teams file list --type pdf --since 30d
teams file fetch --message 1795219003436
teams changes --since 7d
teams calls --missed
teams meetings --since today
teams chat export "Project" --out ~/project.md

teams msg send 48:notes "**Shipped!**" --markdown --file ./report.pdf --image ./shot.png --dry-run
teams msg react 48:notes/1795219003436 👍
teams msg edit 48:notes/1795219003436 "Updated copy" --dry-run
teams msg forward 1795219003436 48:notes --comment "FYI" --dry-run
teams chat new alice@example.com bob@example.com --topic "Launch" --dry-run
teams chat read "Project"
teams presence
teams presence set dnd --for 2h
teams presence message set "Heads down" --for 2h
```

Add `--json` to commands for machine-readable output matching the HTTP API.

Exit codes: `0` ok, `1` runtime/health/auth/policy error, `2` usage/parameter error.

## Local HTTP API & Scoped Tokens

The background daemon (`teams daemon install`) listens on `http://127.0.0.1:47613` (`TEAMS_CLI_PORT` to override) and checks for cache changes every 60s.

- `GET /health` is unauthenticated.
- All `/v1/*` routes require `Authorization: Bearer <token>` and reject non-loopback `Host`, `Origin`, and cross-site `Sec-Fetch-Site` headers.
- Every named token has explicit scopes (defaulting to `read` only) and an optional exact-ID chat allowlist (`--chats`) that restricts live chat write actions (archive reads are not chat-filtered).
- Live write routes under `/v1/live/*` require both `--allow-writes` on the daemon and a token carrying the exact scope (`send`, `modify`, `react`, `mark-read`, `chat:manage`, `presence:write`).

```sh
teams token create reader --expires 30d
teams token create executor --scope read,sync --expires 90d
teams token create notifier --scope read,send,react --chats "Engineering,48:notes" --expires 30d
teams token list
teams token revoke notifier
teams token prune
```

```sh
TOKEN=$(teams token create reader --expires 30d)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:47613/v1/status
```

An optional typed Executor wrapper lives in `executor-app/` (`executor-app/deploy.sh`).

## Operations

```sh
teams daemon status | restart | uninstall | logs [--follow] | run [--allow-writes]
teams archive verify | migrations | reindex
teams archive backup --out ~/teams-backup.sqlite3.xz
teams archive restore ~/teams-backup.sqlite3.xz --yes
teams debug stores | dump | records
```

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

---

Not affiliated with, endorsed by, or sponsored by Microsoft Corporation.
