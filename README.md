# teams-cli

Local-first archive, CLI, and loopback HTTP API for Microsoft Teams (v2) on macOS.

Reads the Teams desktop IndexedDB/LevelDB cache into an append-only SQLite archive (`~/Library/Application Support/teams-cli/archive.sqlite3`) so messages, edits, deletions, attachments, calls, and meetings are kept permanently even after Teams evicts them.

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

Update anytime with `teams update` (`teams update --check` to check without installing). Interactive commands check GitHub releases at most once every 24 hours and show a notice when a newer version is available (set `TEAMS_CLI_NO_UPDATE_CHECK=1` to disable). Prebuilt binaries and `SHA256SUMS.txt` are on [GitHub Releases](https://github.com/sasanktumpati/teams-cli/releases); verify provenance with `gh attestation verify teams-*.tar.gz -R sasanktumpati/teams-cli`.

On first run, allow macOS to access `~/Library/Containers/com.microsoft.teams2/`.

### Shell Completions

```sh
teams completions zsh  > "${fpath[1]}/_teams"
teams completions fish > ~/.config/fish/completions/teams.fish
teams completions bash > ~/.local/share/bash-completion/completions/teams
```

## Usage

Run `teams docs` for the full command, flag, and JSON schema reference.

```sh
teams status
teams doctor
teams sync

teams inbox
teams mentions --unread
teams chats --grep "project"
teams chats --member alice@example.com
teams messages "Project" --since 7d
teams messages --around 1795219003436 --context 10

teams search "launch plan" -draft --from Bob --since 30d
teams search '"exact phrase" OR other' --relevance
teams search budget --include-history
teams search --has-attachments --since 30d
teams search --from Alice --count

teams people --grep alice
teams people alice@example.com
teams attachments --type pdf --since 30d
teams changes --since 7d
teams calls --missed
teams meetings --since today
teams saved
teams pinned
teams export "Project" --out ~/project.md

teams fetch --message 1795219003436
teams send 48:notes "hello" --dry-run
```

Add `--json` to any read command for machine-readable output matching the HTTP API.

Exit codes: `0` ok, `1` runtime/health error, `2` usage/parameter error.

## Local HTTP API & Auth

The background daemon (`teams daemon install`) listens on `http://127.0.0.1:47613` (`TEAMS_CLI_PORT` to override) and checks for cache changes every 60s.

- `GET /health` is unauthenticated.
- All `/v1/*` routes require `Authorization: Bearer <token>` and reject non-loopback `Host`, `Origin`, and cross-site `Sec-Fetch-Site` headers.
- All endpoints are read-only except `POST /v1/sync`. `fetch` and `send` are CLI-only (`send` requires both `TEAMS_CLI_ENABLE_SEND=1` and `--yes`).

```sh
teams token create my-script --expires 30d
teams token list
teams token revoke my-script
teams token prune
```

```sh
TOKEN=$(teams token create my-script --expires 30d)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:47613/v1/status
```

An optional typed Executor wrapper lives in `executor-app/` (`executor-app/deploy.sh`).

## Operations

```sh
teams daemon status | restart | uninstall | logs [--follow] | run
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
