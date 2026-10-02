# Queryfolio

SQL client desktop app. A lightweight, multi-purpose SQL GUI client.

One app for several database engines: **MySQL / PostgreSQL / SQLite / DuckDB** on the SQL side,
plus **Redis / Elasticsearch / DynamoDB**. The editor language and the unit of execution switch
per engine, and each engine declares what it supports, so features it lacks are hidden from the
UI rather than left as dead buttons.

https://github.com/user-attachments/assets/90439816-49c8-4ebd-a068-b102cfe9c7aa

![Queryfolio screenshot](docs/screenshot.png)

## Features

- MySQL / PostgreSQL / SQLite support (via sqlx)
- Redis support (`engine: redis`): one command per line (`GET my-key`, `MGET a b c`) with syntax highlighting, a read-only command whitelist while the Writable switch is off, and guards for destructive commands (`FLUSHALL` etc.)
- Elasticsearch support (`engine: elasticsearch`): Kibana-Console-style request blocks (`GET /index/_search` + JSON body, NDJSON `_bulk`), hits rendered as a table, an index browser with mapping fields, and guards for destructive requests (index deletion, `_delete_by_query` etc.)
- DuckDB support (`engine: duckdb`): a local-file SQL engine (like the SQLite support) with the full SQL feature set (meta commands, EXPLAIN, auto LIMIT incl. `FROM`-first queries)
- DynamoDB support (`engine: dynamodb`): PartiQL statements in the SQL editor, a table browser with key attributes, AWS credentials via static keys / `aws_profile` / the default chain, and the same read-only / dangerous guards
- SSH tunnel with known_hosts verification
- Connection config in YAML, compatible with the sql-agent-mcp-server format
  - Secrets can stay in 1Password: the config YAML is fetched lazily via a getter command like `op read "op://..."`
  - Per-connection `password_command`: fetch a password when that connection is opened (e.g. AWS RDS IAM auth tokens), with one automatic retry when an expired token is rejected (PostgreSQL / MySQL)
- Query files per connection, auto-saved (`~/.config/queryfolio/sqlfiles/<folder>/*.sql`, where `<folder>` is `folder_name` or `<host>_<engine>_<schema>_<user>`)
- CodeMirror 6 SQL editor with per-engine dialect, statement highlighting, Cmd+Enter to run the statement under the cursor, and schema-based autocompletion (table / column names; Tab or Enter accepts a suggestion)
- SQL formatting for SELECT statements (conservative: unsupported or unparsable statements are left untouched)
- Schema browser (TABLES pane) with lazy-loaded columns
- Results in tabs with pinning, a cell inspector, and CSV / TSV / JSON copy (formula-injection safe)
- Query cancellation while running
- Per-connection query history (searchable, stored locally with restrictive file permissions)
- psql-style meta commands (`\l` `\dt` `\dv` `\dn` `\du` `\d [table]`) translated to catalog queries, with MySQL / SQLite equivalents where possible
- `\c <database>` (or `USE <database>`) switches the active database of the connection (MySQL / PostgreSQL). The pool is rebuilt, and the database selector, schema browser, and SQL completion follow. Both are connection state changes rather than statements, so they work with the Writable switch off
- `readonly: true` per connection rejects write statements (INSERT / UPDATE / DELETE / DDL, including CTE-wrapped DML) as a safety guard
- TLS for MySQL / PostgreSQL: `tls: true` requires an encrypted connection **and verifies the server certificate** (`verify-full`); `ssl_mode` (`disable` / `prefer` / `require` / `verify-ca` / `verify-full`) and `ssl_root_cert` give finer control. **Without either, the driver default (`prefer`) applies: it falls back to plaintext when TLS cannot be negotiated and does not verify the certificate** — hover a connection to see its `TLS` value in the details tooltip
- Auto `LIMIT` for SELECTs without one (`default_limit`, default 500)
- AI features (OpenAI): SQL generation from natural language, Fix with AI on query errors, EXPLAIN plan analysis with index suggestions, and explanation of selected SQL. Generated SQL is inserted into the editor, never auto-executed. Only the schema (table / column names), engine dialect, statements, and plans are sent — never query results
- AI chat pane (right side, toggled from the toolbar): ask about your data in a conversation. The assistant can run SQL itself to look things up, through a **read-only guard** that ignores the Writable switch (`CALL`, `PRAGMA`, multi-statement input, and anything but read statements are rejected), and every query it ran is shown with the answer. On top of that check, the database enforces read-only itself (`BEGIN READ ONLY` on PostgreSQL / MySQL / DuckDB, `PRAGMA query_only` on SQLite), so a `SELECT` that calls a side-effecting function such as `nextval()` is refused by the server too. Enforcement covers non-temporary objects; temp tables are exempt on both PostgreSQL and MySQL, MySQL DDL escapes the transaction via implicit commit (the statement whitelist is what stops it there), writes to *other* systems (`dblink` and the like) are out of reach, and the assistant can read everything the connection can — use read-only credentials when that matters. Code blocks have Copy / Insert buttons; rows it read are sent back to the model as tool results, so keep the pane closed when that is not acceptable
- Resizable panes: drag the dividers between the sidebars, editor, and results; sizes are persisted across restarts
- Window size / position restored across restarts
- Open a saved query file by path from a `queryfolio://open/<path>` URL or the `queryfolio open <path>` CLI subcommand (restricted to files under the query files directory; reuses the running window)
- Write and open a query file from the CLI with `queryfolio write <connection> <file-name> [content]` (content can also be piped in on stdin) — for AI agents that prepare a query for review
- Inspect the configuration without launching the app: `queryfolio --help`, `queryfolio --version`, `queryfolio --list-servers` (`queryfolio --license` prints the third-party licenses)

## Setup

```shell
pnpm install
pnpm tauri dev
```

## Configuration

Everything lives in one file: `~/.config/queryfolio/config.yml` (see `config.example.yaml`). Connections are written under `servers`, and any key can be overridden from an external source with `config_override_command`:

> 📖 **See [docs/settings.md](docs/settings.md) for the full settings reference** — every key, SSH tunnel modes, groups, templates, `config_override_command`, AI, and more.

```yaml
# Inline (entry format is sql-agent-mcp-server compatible)
servers:
  - name: dev-postgres
    engine: postgres
    host: localhost
    readonly: true   # optional: reject write statements on this connection
    ...

# Servers can be grouped for the connections list (queryfolio extension).
# Group entries and plain servers can be mixed; order is preserved.
# servers:
#   - group_name: production
#     servers:
#       - name: prod-main
#         ...
#   - name: ungrouped-db
#     ...

# Optional: run a command whose stdout is YAML, and merge it over this file.
# Mappings are merged recursively; scalars and lists (including servers)
# are replaced wholesale. Any key can be overridden this way.
# config_override_command: op read "op://development/queryfolio/config-yaml"

# Optional
sqlfiles_dir: ~/queries
default_limit: 500   # auto-appended to SELECTs without LIMIT (0 = disabled)

# Optional: AI SQL generation (OpenAI)
ai:
  provider: openai   # only openai is supported for now
  api_key: sk-your-api-key
  model: gpt-6-luna     # optional (default: gpt-6-luna)
  base_url: https://api.openai.com/v1  # optional (for OpenAI-compatible APIs)
```

The `ai:` section can live at the top level of the local config file, or at the top level of the YAML fetched by `config_override_command`. The fetched YAML wins (that is just the merge result) — so the API key can stay in 1Password together with the connection secrets.

The `QUERYFOLIO_CONFIG_YAML` environment variable overrides the whole config file (for development; GUI apps launched from Finder do not inherit shell env vars).

## Opening files by URL / CLI

A saved query file can be opened by path, from either a URL scheme or a CLI subcommand. Both go through the same router and reuse the already-running window.

```shell
# URL scheme (absolute path; the leading slash of the path doubles the slash after the scheme)
open "queryfolio://open//Users/me/.config/queryfolio/sqlfiles/reporting/monthly.sql"

# CLI subcommand
queryfolio open /Users/me/.config/queryfolio/sqlfiles/reporting/monthly.sql
```

Only files directly under a connection folder inside the query files directory (`sqlfiles_dir`) can be opened. Paths outside that directory, unknown folders, non-`.sql` files, and `..` traversal are rejected. The connection that owns the folder is selected automatically and the file is opened in a new editor tab.

### Writing a query file from the CLI

`write` takes a connection name and a file name instead of a path, so the query files directory and the connection's folder name do not have to be known by the caller. The content can be given as the third argument or piped in on stdin. This is mainly meant for AI agents that prepare a query for a human to review and run.

```shell
# Content as an argument
queryfolio write reporting monthly.sql "SELECT * FROM orders LIMIT 10;"

# Content on stdin
echo "SELECT * FROM orders LIMIT 10;" | queryfolio write reporting monthly.sql

# No content: create the file if it does not exist yet, then open it
queryfolio write reporting monthly.sql
```

- The connection name is the `name` of a server in the config; the file gets the connection engine's extension (`.sql` / `.redis` / `.es`) if it is missing, and the connection's folder is created if needed.
- Content is written only when it is actually given. An **empty** stdin is treated as "no content" so that a GUI launch (`open -a Queryfolio --args write ...`, whose stdin is `/dev/null`) cannot silently blank an existing file. Existing content is otherwise overwritten.
- The file is written by the process you launch, before the running window is asked to open it — stdin cannot be forwarded to an already-running instance. If writing fails (unknown connection, invalid name, stdin larger than 10 MiB, I/O error), the reason is printed to stderr and the command exits non-zero without opening anything.
- `write` is **CLI-only**: there is no `queryfolio://write/...` URL. A web page can make the browser open a `queryfolio://` URL, and dropping arbitrary SQL into the query files directory that way would be a trap waiting for the next person who runs it.

### Inspecting the configuration from the CLI

These options print to stdout and exit without launching the app or touching an already-running window.

```shell
queryfolio --help           # usage, including the open / write subcommands
queryfolio --version
queryfolio --license        # licenses of the bundled third-party libraries
queryfolio --list-servers   # the configured connections
```

`--list-servers` prints the resolved query files directory, then one row per connection: name, engine, host, port, user, database, TLS, whether an SSH tunnel is used, and the query file folder. **Passwords and SSH keys or passphrases are never printed** — the row is built from the same non-secret projection (`ConnectionInfo`) that is handed to the frontend.

The TLS column shows the effective mode for mysql / postgres / redis (`disable` / `prefer` / `require` / `verify-ca` / `verify-full`) rather than a yes/no, because the default `prefer` falls back to plaintext without verifying the certificate; collapsing it into "enabled" would hide that. Other engines show the `tls` flag as `on` / `off`. A connection shows `invalid` when it cannot connect at all with the setting it has: an unresolvable `engine`, an unsupported `ssl_mode` on mysql / postgres, or an `ssl_root_cert` combined with a mode that does not verify it (`disable` / `prefer` / `require`, including the `prefer` you get by leaving `ssl_mode` out) — that combination is rejected rather than silently ignored. Whether the certificate file exists is not checked here. An unsupported `ssl_mode` on an engine that never reads it (elasticsearch / sqlite / duckdb / dynamodb) is not reported as `invalid`, because those connections work — `invalid` means "this will not connect", not "there is a stray value in the config". A dynamodb connection without a `host` shows the scheme the AWS SDK will actually use: `host` / `port` / `tls` there override the endpoint for dynamodb-local, and without that override the SDK resolves the regional endpoint over HTTPS unless `AWS_ENDPOINT_URL_DYNAMODB` / `AWS_ENDPOINT_URL` point somewhere else (both are honoured here, as is `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS`). An `endpoint_url` set in `~/.aws/config` rather than in the environment is not read, so such a connection is shown as `on`.

The USER column shows `(hidden)` for dynamodb, because that `user` is the AWS access key ID rather than a database user name. It is deliberately not the same as `-` (not configured): a connection using static keys and one falling back to `aws_profile` or the default credential chain are different things to know about.

The config is read the same way the app reads it, including `config_override_command`, so this also works when the connections come from an external command rather than from `config.yml`.

On Windows the release build is linked as a GUI application and has no console of its own, so these commands attach to the console of the process that started them before printing. Redirecting to a file or a pipe works as usual.

Two consequences of that remain, and they are deliberate rather than oversights: because the shell does not wait for a GUI-subsystem program, in an interactive `cmd.exe` or PowerShell session the output can arrive after the prompt has already been redrawn, and `%ERRORLEVEL%` / `$LASTEXITCODE` will not carry the exit code of the option you ran. Removing them would mean shipping a second, console-subsystem executable in the installer, which puts a console window on every GUI launch. Use `Start-Process -Wait queryfolio -ArgumentList '--list-servers'` when you need the shell to wait.

On macOS, call the binary inside the bundle — `open -a Queryfolio --args --list-servers` does not give you the output back:

```shell
/Applications/Queryfolio.app/Contents/MacOS/queryfolio --list-servers
```

An option is only recognised before a subcommand, so `queryfolio write conn a.sql "-- help"` writes that content instead of printing the help.

## Agent skill

The repository ships a skill that teaches an AI coding agent how Queryfolio stores things on
disk — the query file layout, the connection → folder mapping, the CLI above, the query
history, and the `-- 📝` **Run and Log** marker, which is how an agent gets the result of a
query it cannot run itself: it writes the marked SQL, you run it in Queryfolio, and the result
is written back into the same file as a block comment for the agent to read.

| Skill | What it covers |
|---|---|
| [`queryfolio`](skills/queryfolio/SKILL.md) | Query files, config, history, the CLI, and Run and Log |

It is a plain `skills/<name>/SKILL.md` directory, so any tool that reads that layout can
install it.

### With `npx skills` (Claude Code, Codex, Cursor, OpenCode, …)

```shell
npx skills add cyberneura/queryfolio --list                # see what the repo offers
npx skills add cyberneura/queryfolio --skill queryfolio    # into ./<agent>/skills (this project)
npx skills add cyberneura/queryfolio --skill queryfolio -g # into ~/<agent>/skills (all projects)
```

Use `--skill queryfolio` to install just this one: the repository also carries an internal
release-workflow skill under `.claude/skills/`, which the CLI lists as well but which is only
useful when working on Queryfolio itself.

Installing the skill does not install Queryfolio; see [Install](#install) for that.

## Development

```shell
pnpm check                   # svelte-check
cd src-tauri && cargo test   # Rust unit tests
pnpm tauri build             # release build (macOS: signed with Developer ID via the tauri script)
```

See `AGENTS.md` for architecture details.

## Install

### Homebrew (macOS)

```shell
brew install --cask cyberneura/tap/queryfolio
```

### Manual download

Grab the latest installer from the [Releases page](https://github.com/cyberneura/queryfolio/releases/latest):

- **macOS**: `Queryfolio_<version>_universal.dmg` (Apple Silicon + Intel). Signed with a
  Developer ID certificate and notarized by Apple, so it opens without a Gatekeeper warning.
- **Windows**: `Queryfolio_<version>_x64-setup.exe` (NSIS installer). It is *not* code signed,
  so SmartScreen shows "Windows protected your PC" — choose **More info › Run anyway**.

## Release

Releases are built on GitHub Actions (`.github/workflows/release.yml`) and published as a
GitHub Release. A push to `main` releases the version in `src-tauri/tauri.conf.json` if it is
not published yet and is newer than the latest release; otherwise nothing happens. Bump the
version and push it with one command:

```shell
pnpm release                 # 0.1.0 -> 0.1.1 (patch)
pnpm release minor           # 0.1.0 -> 0.2.0
pnpm release major           # 0.1.0 -> 1.0.0
fab release                  # same thing via Fabric (fab release:minor / fab release:major)
fab -l                       # list all tasks (dev / check / unittest / build_local / release / releases)
```

The script requires a clean `main` in sync with `origin/main`. It bumps the version in
`src-tauri/tauri.conf.json` and `package.json`, pushes the bump commit, follows the run that
push started, and checks that the Release was actually published. The workflow creates one
**draft** Release, builds the macOS universal dmg (Developer ID signed + notarized + stapled)
and the Windows NSIS installer in parallel, uploads both to that draft, and publishes it only
after every platform succeeded (a missing signing
secret fails the macOS job up front, so an unsigned or un-notarized build is never
published). See the `publish-macos-release` skill (`.claude/skills/publish-macos-release/`)
for the full runbook, including how to verify the published dmg and the one-time
signing-secrets setup.

The cask in [cyberneura/homebrew-tap](https://github.com/cyberneura/homebrew-tap)
(`Casks/queryfolio.rb`) follows the latest Release on its own; the tap checks every hour.

## License

MIT. See [LICENSE](LICENSE).

## Third-party licenses

[`THIRD-PARTY-NOTICES.txt`](THIRD-PARTY-NOTICES.txt) lists the licenses of the libraries
bundled into the app: every Rust crate compiled into the macOS and Windows binaries, the C / C++
libraries some of those crates build from source and link statically (OpenSSL, libssh2,
zlib, SQLite, and the libraries DuckDB vendors in its `third_party/`), and the npm packages bundled into the web view (`dependencies` in
`package.json` with their dependencies, plus the Svelte / SvelteKit runtime, svelte-sonner,
Tailwind CSS and Bootstrap Icons). It is compiled into the app and shown by
"Third-Party Licenses" in the application menu on macOS (the Help menu on Windows), and by
`queryfolio --license`. Regenerate it after adding or updating a dependency; the Rust tests
fail if a direct dependency is missing from it or a listed version is not in the lock files.

```shell
cargo install cargo-about --locked --features cli   # once
pnpm notices
```

The script needs network access: cargo-about fetches license files for some crates, and the
licenses of DuckDB's vendored libraries are downloaded from the DuckDB repository at the tag
of the bundled DuckDB version.
