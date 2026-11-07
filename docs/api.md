# HTTP API

`wxapkg-dump --server` 提供一套本地 HTTP 接口，供脚本与其它工具调用。默认只监听回环地址。
交互式文档在 `/docs`（Swagger UI），机器可读契约在 `/openapi.json`。

```bash
wxapkg-dump --server --bind 127.0.0.1:8788 --workspace ~/wxapkg-workspace
```

服务不带任何界面，只有接口与文档。桌面程序（`wxapkg-dump-gui`，也可由 `wxapkg-dump --gui` 启动）直接调用同一套引擎，
不经过这个服务，两者可以各自独立运行。

## 响应形状

除下面标注的三个端点外，**每个**端点都返回同一种信封：

```json
{
  "ok": true,
  "command": "programs",
  "version": "1.0.0",
  "protocol": 1,
  "data": { }
}
```

```json
{
  "ok": false,
  "command": "programs",
  "version": "1.0.0",
  "protocol": 1,
  "error": { "code": "program_not_found", "message": "…", "hint": "…" }
}
```

`protocol` 也出现在每个响应的 `X-API-Protocol` 头里，供调用方在不解析正文的情况下判断兼容性。
`hint` 是可选的：只在用户能自行处理时出现，内容是可以照做的下一步。

错误码是稳定字符串，与 CLI 共用同一份定义，见 [conventions.md](conventions.md#错误码)。

`message` 与 `hint` 的语言跟随服务进程的 `--lang` / `WXAPKG_LANG` / 区域设置（与 CLI 同一套规则），
与命令行共用同一份消息目录表，因此同一个错误在两个入口下措辞一致。**错误码永远不随语言变化。**

### 三个例外

以下端点直接返回**字节**而不是信封，因为它们的目标是浏览器直接消费：

| 端点 | 内容 |
|---|---|
| `GET /api/programs/{appid}/icon` | 图片字节，`content-type` 按签名判定 |
| `GET /api/programs/{appid}/raw/{name}` | 容器内条目原文 |
| `GET /api/project/download?path=` | 归档流，带 `content-disposition: attachment` |

它们**失败时仍然返回信封**（例如 404 + `{"ok":false,…}`），所以调用方要先看状态码再决定是否解析
正文。`raw` 的 `name` 可以含 `/`（如 `components/list/list.html`），整体作为一个路径段编码。

## 鉴权

不设置 `--admin-key` 时只允许绑定回环地址。设置后：

- 服务索引（`/`）与接口文档（`/docs`）**不需要**令牌——顶层导航无法携带自定义头，且两者
  不含机密。其余全部端点都在 `/api/` 下，一律受保护。
- `/api/*` 接受 `x-admin-key` 请求头，或 `?key=` 查询参数。后者是给 `EventSource` 与
  `<img src>` 用的，它们无法设置请求头。
- 令牌错误或缺失返回 `401` + `code: "unauthorized"`。

## 本机目录与清单

### `GET /api/health`

进程存活、版本与工作区。用于探活。

### `GET /api/programs`

列出本机已安装的小程序。

| 参数 | 说明 |
|---|---|
| `appid` | 只返回该小程序；**剪枝发生在遍历过程中**，因此明显更快 |
| `roots` | 可重复，覆盖默认搜索根 |
| `limit` | 最多返回这么多**程序**；在分组之后截断，**不减少遍历成本** |
| `refresh` | `true` 时绕过缓存 |

响应：

```json
{
  "count": 84,
  "cached": false,
  "cacheAgeSeconds": 0,
  "skippedPaths": 0,
  "programs": [
    {
      "appid": "wx069ba97219f66d99",
      "version": "17",
      "wxid": "wx069ba97219f66d99",
      "location": "/…/applet/packages/wx069ba97219f66d99/17",
      "user": "…",
      "encrypted": true,
      "totalSize": 123456,
      "mainBundle": "__PLUGINCODE__.wxapkg",
      "modifiedAt": 1792008946,
      "packages": [{ "name": "…", "path": "…", "size": 123, "encrypted": true }],
      "extracted": false,
      "projectDir": null
    }
  ]
}
```

几点需要注意：

- **`skippedPaths` 大于 0 表示列表可能比实际短。** 遍历遇到读不了的子树会跳过而不是失败（微信
  客户端会并发改写这棵树），所以这个数字必须显示出来，否则一个残缺的列表看起来是完整的。
- `projectDir` 与 `extracted` 绑定：未导出时前者为 `null`。「已导出」只认
  `<workspace>/<appid>/<version>/project` 这一约定路径。
- 结果有 30 秒 TTL 缓存，键为实际遍历的 `roots` + `appid` + `limit`。
- 全量遍历本机语料是**秒级到十几秒级**的操作（取决于语料规模与磁盘），别设短的客户端超时。

### `GET /api/scan`

旧版清单，字段较少（`packages` 是**数字**而不是数组）。新代码请用 `/api/programs`。

### `GET /api/environment`

版本、协议、目标平台、远程引擎是否就绪、各类目录、每个搜索根的存在性、缓存账号数、运行时长。

`diskSpace` **恒为 `null`**：跨平台读取可用空间需要额外的文件系统依赖，本构建有意不携带。
`cachedAccounts` 在未编译远程下载支持时为 `null`。

程序数量请调用 `/api/programs`；本接口只做快速环境探测，不遍历语料。

### `GET /api/version`

`{ "version": "1.0.0", "protocol": 1 }`。

## 导出与工程

### `POST /api/programs/export`

定位 → 解包 → 可选美化 → 可选校验，一次调用完成。

```json
{
  "appid": "wx069ba97219f66d99",
  "out": "/tmp/out",
  "beautify": true,
  "verify": true,
  "options": { "keepRaw": true, "decompileWxml": true }
}
```

`appid` 与 `paths` 二选一。工程固定落在 `<out>/<appid>/`；`out` 省略时即
`<workspace>/<appid>/`。`projectDir` 返回实际路径，`out` 只是基目录。
`beautify` 接受布尔开关（`true` 用默认格式）或格式选项对象（如 `{ "indentWidth": 4, "useTabs": true }`）。

响应含 `program`、`report`、`projectDir`、`projects`、`artifacts` 与 `verification`。
`paths` 跨多个 appid 时 `projects` 列出每个工程（`outDir` / `files` / `warnings`），`projectDir`
指向它们的共同基目录；`verification` 仅在单工程且 `verify: true` 时非 null。

`report.counts` 的每一项都是**实际写盘的文件数**，其各项之和等于 `report.files` 的长度，也等于
工程目录条目数减一（不含 `_unpack_report.json` 自身）。这一点在 [`_unpack_report.json`](#落盘报告) 里同样成立。

### `POST /api/programs/export-batch`

```json
{ "appids": ["wx…", "wx…"], "concurrency": 2, "beautify": true, "verify": true }
```

`concurrency` 取 1–4。逐个发布 SSE 事件，返回 `results`、`succeeded`、`failed`。单个程序失败不会
中断整批。

### `POST /api/unpack` / `POST /api/inspect` / `POST /api/raw` / `POST /api/beautify`

`beautify` 的 `dir` 与 `verify` 一样：相对路径按 workspace 解析，绝对路径原样使用。

与 CLI 同名子命令一一对应，载荷与 CLI 参数同义。`options` 的键名是 camelCase。

远程下载的落盘结构：`<out>/original_wxapkg/<appid>/wxapkg/`，`unpack` 后工程在
`<...>/unpack_<appid>/`。

`/api/unpack` 的 `paths` 跨多个 appid 时，每个小程序各还原到 `<out>/<appid>/`，响应为
`{ "projects": [BuildReport…] }`；只有一个程序时仍然直接返回 `BuildReport`，旧客户端不受影响。

### `POST /api/verify`

`dir` 可以是绝对路径，也可以是相对 workspace 的路径。`{ "dir": "…", "node": true }` → 与 `wxapkg-dump verify` 相同的报告（`ok`、`errors`、`warnings`、
`stats`、`syntaxChecked`）。`node` 为真时用 `node --check` 做真实语法解析；Node 不可用不会失败，
而是在 `warnings` 里留下 `node_unavailable`。

### `POST /api/project/zip`

`{ "dir": "…", "includeRaw": false }` → `{ "zipPath", "bytes", "files" }`。

### `GET /api/project/download?path=`

流式返回归档（分块，不整文件缓冲）。**只允许 `.zip` / `.wxapkg`，且必须在 workspace 内。**

### `GET /api/project/tree?dir=&depth=`

`depth` 默认 2、上限 12；条目上限 5000，超过时 `truncated: true`。

**`truncated` 必须显示出来。** 它是「还有内容没列出来」的唯一信号，丢掉它会让页面看起来完整。

### `GET /api/project/file?path=`

上限 2 MiB；含 NUL 字节时以 `415 binary_file` 拒绝。返回 `{ path, size, lines, content }`。

### `POST /api/reveal`

在文件管理器中定位给定路径。macOS 用 `open -R`，Windows 用 `explorer /select,`，其它平台
`xdg-open` 打开父目录。响应回显规范化后的路径（macOS 上 `/tmp` 会变成 `/private/tmp`）。

## 缓存管理

### `GET /api/cache`

列出**本服务创建**的目录及其占用：`projects[]`、`tempDirs[]` 与各项合计。

### `DELETE /api/cache?scope=projects|all&confirm=true`

`scope=all` 必须显式带上 `confirm=true`，否则返回 `400 confirmation_required`。删除前会二次校验
规范化路径，**永不删除 workspace 根**。

## 远程下载（`/api/ilink`）

远程接口只在编译了 `remote` 特性时存在。路径统一为 `/api/ilink/`。除图标与二维码外，全部返回统一信封。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/ilink/status` | 引擎实现、当前账号（`null` 表示未登录）、过期警告 |
| `POST` | `/api/ilink/login/qr` | 发码；请求体可为空，返回 `sessionId`、`qrContent`、`qrImage`、`expiresIn` |
| `POST` | `/api/ilink/login/check` | 轮询；请求体 `{"sessionId":"…"}`，返回 `waiting` / `scanned` / `confirmed` / `expired` / `cancelled` |
| `GET` | `/api/ilink/recent?page=1&size=30` | 最近使用，返回 `total`、`page`、`size`、`items` |
| `POST` | `/api/ilink/info` | 请求体 `{"appid":"wx…"}` 或 `{"username":"gh_…@app"}` |
| `POST` | `/api/ilink/download` | 下载；可同时 `unpack` / `beautify` |
| `DELETE` | `/api/ilink/account` | 删除当前账号及其缓存，返回 `{"deleted":true}` |
| `GET` | `/api/ilink/icons/{name}` | 缓存的图标字节 |
| `GET` | `/api/ilink/qr.svg?content=…` | 把二维码内容渲染为 SVG |

`login/check` 的 `confirmed` 响应同时带上 `nickName`、`avatar`、`openid`，调用方无需再查一次
`status`。二维码内容在 `confirmed` 之前有效；过期后重新调用 `login/qr`。

`download` 请求体：

```json
{
  "appid": "wx15f182efdb0ba66c",
  "out": "/tmp/remote",
  "versionType": 0,
  "version": 0,
  "allPackages": false,
  "concurrency": 3,
  "unpack": true,
  "beautify": true
}
```

`appid` 与 `username` 至少给一个；`versionType` 为 `0` 正式、`1` 体验、`2` 开发、`999` 预览；
`concurrency` 取 1–10（默认 3）；`allPackages` 会连资源全量包一起下载，体积可能翻数倍。

响应中的 `outDir` 是容器目录，`packages` 逐包给出 `module`、`type`、`size`、`path`、`error`；
`unpackedDir` 是 `--unpack` 后的人类可读摘要（路径 + 文件数 + 警告数），不是单独字段。

```json
{
  "ok": true,
  "command": "ilink download",
  "version": "1.0.0",
  "protocol": 1,
  "data": {
    "appid": "wx15f182efdb0ba66c",
    "username": "",
    "outDir": "/tmp/remote/original_wxapkg/wx15f182efdb0ba66c/wxapkg",
    "packages": [{ "module": "__APP__", "type": 4, "size": 8095474, "path": "…", "error": "" }],
    "unpackedDir": "/tmp/remote/…/unpack_wx15f182efdb0ba66c (1380 files, 32 warnings)"
  }
}
```

## 事件流

### `GET /api/events`

Server-Sent Events。每条事件的 `data` 是一行 JSON：

```
data: {"ts":1792008946,"level":"info","message":"corpus walk finished: 84 program(s) in 1180ms"}
```

`level` 为 `info` | `warn` | `error`。导出、批量导出与缓存操作都会发布事件，前端日志抽屉消费
它。`EventSource` 无法设置请求头，因此这个端点必须用 `?key=` 传令牌。

关闭服务时总线先关闭、再发布最后一行，订阅者的流随之结束——否则 `with_graceful_shutdown` 会一直
等待一个永不结束的响应体。

## 落盘报告

每次解包都会在输出目录写 `_unpack_report.json`：

```json
{
  "generatedBy": "wxapkg-dump 1.0.0",
  "outDir": "…",
  "parts": [{ "file": "…", "kind": "main", "root": "/", "version": 0, "files": 4 }],
  "counts": { "js": 6, "wxml": 1, "json": 5, "raw": 3, "…": 0 },
  "files": ["app.json", "…"],
  "notes": [],
  "warnings": []
}
```

它**不在** `files` 列表里，因为它是报告而非产物。

## 错误处理约定

- 路径参数一律过 workspace 校验，`../` 逃逸返回 `403 forbidden`。
- 路由段里的 `..` 即使经过 URL 编码也会被拒。
- 未知 appid 在导出时返回 `400 program_not_found` 并附带 hint；在 `/api/programs?appid=` 过滤时
  返回 `200` 与空列表——过滤不是查找。
