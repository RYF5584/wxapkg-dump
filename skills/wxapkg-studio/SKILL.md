---
name: wxapkg-studio
description: WeChat Mini Program package toolkit. Use when the user wants to find, extract, decompile, beautify or download WeChat Mini Programs (小程序 / wxapkg) — for example scanning the local WeChat client cache, turning a .wxapkg into a project the WeChat DevTools can open, recovering WXML/WXSS/JavaScript sources, or fetching a Mini Program's packages with an authorized session. Also handles V1MMWX-encrypted PC/macOS bundles. Triggers: wxapkg-dump, 小程序源码, 小程序反编译, 小程序解包, 小程序包, 微信小程序, 小程序下载, 应用宝, V1MMWX, app-service.js, $gwx, setCssToHead.
---

# wxapkg-studio

Extract, decompile, beautify and download WeChat Mini Program packages. Four front-ends
share one engine, so results are identical whichever you drive:

| Surface | Entry point | Best for |
|---|---|---|
| CLI | `wxapkg-dump <subcommand>` | precise, scriptable work |
| Desktop GUI | `wxapkg-dump-gui` | interactive browsing and batch work |
| HTTP API | `wxapkg-dump --server` | other tools, CI |
| MCP | `wxapkg-dump --mcp` | assistant tool calls over stdio |

The desktop app is `wxapkg-dump-gui`. `wxapkg-dump --gui` starts it from the CLI when the
binary or app bundle is next to the CLI (or named by `WXAPKG_GUI`).

## Before you act

```bash
wxapkg-dump --version
wxapkg-dump doctor --json     # paths, remote storage, search roots
```

If the binary is missing, build it with `cargo build --release -p wxapkg-cli`. Do not
substitute another tool: the output layout (a DevTools-importable project) is specific to
this one.

Messages follow the system locale. Pass `--lang zh-Hans` or `--lang en` to force one.

## Local extraction

### 1. Find cached programs

```bash
wxapkg-dump scan --json
wxapkg-dump scan --appid wx9fdab02115681f58 --json
wxapkg-dump scan --path "/custom/wechat/data" --json
```

Reports appid, version directory, bundle count, total size, V1MMWX encryption and paths.
WeChat 3.x and 4.x layouts on Windows and macOS are covered, with a recursive fallback.

### 2. Inspect before unpacking

```bash
wxapkg-dump info <package-or-directory> --json
```

Cheap and non-destructive: container format, index/body sizes, file count and the pages,
sub-packages and plugins declared in `app-config.json`. Prefer this when the user only needs
structure.

### 3. Reconstruct a project

```bash
wxapkg-dump unpack --appid wx9fdab02115681f58 -o ./out --beautify --json
wxapkg-dump unpack ./main.wxapkg ./sub_a.wxapkg -o ./out --json
wxapkg-dump unpack ./export.zip -o ./out --json
wxapkg-dump unpack --appid wx… --raw ./raw --json     # verbatim container entries
```

`--appid` locates the package automatically; otherwise pass `.wxapkg` files, `.zip` archives
or directories. A file or folder picked by the user restores beside it as
`unpack_<appid>/`; a folder holding several appids produces one project per appid.

The result is a project the official WeChat DevTools can import:

```text
out/wx…/
├── app.js / app.json / app.wxss
├── project.config.json / sitemap.json
├── pages/ components/ miniprogram_npm/
├── _raw/                  original containers, excluded from packing
└── _unpack_report.json
```

Useful options (full list in `docs/cli.md`):

- `--plugins keep|placeholder|strip` — how to treat plugin declarations (default `keep`).
- `--no-auto-fix` — faithful mode: no page/sub-package pruning, no config repair.
- `--no-raw`, `--no-wxml`, `--no-require-rewrite`, `--no-prune`, `--no-synthesize`,
  `--no-plugin-clean` — disable one pass at a time.
- `--beautify [--indent N] [--use-tabs]` — format everything after reconstructing.
- `--strict` — treat warnings as failure (exit 1).

### 4. Beautify or verify an existing project

```bash
wxapkg-dump beautify ./out/wx… --json
wxapkg-dump verify ./out/wx… --node --json
```

`beautify` changes formatting only. `verify` checks structure, references, WXML/WXSS syntax
and, with `--node`, real JavaScript syntax.

## Authorized remote download

Remote access needs a session the **user** establishes by scanning a QR code. Never fabricate
credentials.

### Check state first

```bash
wxapkg-dump account-status --json
```

If no account is signed in, ask the user to run this in a terminal and scan the QR with
WeChat:

```bash
wxapkg-dump login --qr-out /tmp/qr.png
```

For a non-interactive caller, split the flow:

```bash
wxapkg-dump login get-qr --json --qr-out /tmp/qr.png   # returns the session and QR payload
wxapkg-dump login check-qr --session <key> --json      # poll once; a scan is exchanged here
```

Report the QR payload/path and let the user decide when to continue. Do not loop on
`check-qr` unless the user asked for it.

### Find a target

```bash
wxapkg-dump recent --page 1 --size 20 --json
wxapkg-dump remote-info --appid wx… --json
wxapkg-dump remote-info --username 'gh_xxx@app' --json
```

`recent` returns nickname, icon, last-used time and the original id. Keep the `@app` suffix
on the original id verbatim. The `appid` field is sometimes empty in usage records; use
`--username` in that case.

### Download

```bash
# release channel, then reconstruct and beautify in one step
wxapkg-dump download --appid wxe30a622cc9cdc70b -o ./dl --unpack --beautify --json

# by original id
wxapkg-dump download --username 'gh_7d2ae877326d@app' -o ./dl --unpack --json

# trial / development / preview
wxapkg-dump download --appid wx… -o ./dl --version-type 2 --unpack --json

# include resource widgets (fonts, maps, animations)
wxapkg-dump download --appid wx… -o ./dl --all-packages --unpack --json
```

`--version-type`: `0` release, `1` trial, `2` development, `999` preview. The result keeps
every `.wxapkg` and a `manifest.json` under `original_wxapkg/<appid>/wxapkg/`; the project is
`unpack_<appid>/` beside them.

A non-empty `error` for one package means that sub-package failed while the rest remain
usable: report partial success, not a blanket failure. Payloads that are not valid wxapkg
files are saved with an `.invalid` suffix and explained in the report.

### Delete the account

```bash
wxapkg-dump account-delete --yes --json
```

Only when the user explicitly asks. This removes the session and caches tied to it.

## MCP

```bash
wxapkg-dump --mcp
```

Nine tools over JSON-RPC 2.0 on stdio:

| Tool | Purpose |
|---|---|
| `wxapkg_scan_local_programs` | list cached programs |
| `wxapkg_inspect_package` | inspect a container |
| `wxapkg_unpack_package` | reconstruct a project |
| `wxapkg_raw_dump` | export container entries verbatim |
| `wxapkg_beautify_project` | format a project |
| `ilink_account_status` | signed-in account |
| `ilink_recent_mini_programs` | recently used programs |
| `ilink_basic_info` | remote attributes |
| `ilink_download_mini_program` | download packages and optionally restore |

MCP cannot show a QR code. If `ilink_account_status` reports no session, ask the user to run
`wxapkg-dump login` first.

## HTTP API

```bash
wxapkg-dump --server --bind 127.0.0.1:8788
# docs: http://127.0.0.1:8788/docs
```

Core routes: `GET /api/scan`, `POST /api/inspect`, `POST /api/unpack`, `POST /api/raw`,
`POST /api/beautify`, `POST /api/verify`, `GET /api/events` (SSE), `GET /api/version`.
Remote routes live under `/api/ilink/`: `status`, `recent`, `info`, `download`, `account`,
`login/qr`, `login/check`.
Binding a non-loopback address requires `--allow-remote`; set `--admin-key` as well.

## Output contract

```json
{ "ok": true,  "command": "unpack", "version": "1.0.0", "data": { } }
{ "ok": false, "command": "unpack", "version": "1.0.0",
  "error": { "code": "program_not_found", "message": "…", "hint": "…" } }
```

Always branch on `ok`, and show `error.message` with `error.hint` when present; the hints are
written to be shown verbatim.

Exit codes: `0` success, `1` warnings under `--strict`, `2` bad argument or input,
`3` runtime failure, `4` not signed in / session expired, `5` upstream rejection.

## Reporting results

- Name the appid, output directory and file counts by category.
- Surface warnings. A high count of "the operand array is empty" means the package ships no
  template operands (Skyline/Taro builds); the structure is correct but bound expressions
  are `undefined`. Say so rather than implying a clean result.
- Do not claim a Mini Program "works" after unpacking. Payment, remote config and cloud
  functions need their servers and cannot function offline.

## Boundaries

Extracting or decompiling a Mini Program you do not own or have not been authorised to
analyse is not acceptable use. Prefer `wxapkg-dump info` when only structure is needed, and
decline bulk extraction of third-party assets. Never present a stale local cache as current
remote state — call `wxapkg-dump account-status`.
