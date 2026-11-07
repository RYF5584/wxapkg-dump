# 规范

本文件是仓库的单一事实来源：目录、缓存、环境变量、命名、错误码、日志、输出约定。新增代码前先读这里；任何与实现不一致的地方，以本文件为准并同时改实现。

## 1. 文件系统布局

### 用户目录

平台标准位置，不在仓库里，不随二进制走：

| 角色 | macOS | Linux | Windows |
|---|---|---|---|
| 配置 | `~/Library/Application Support/wxapkg-dump` | `$XDG_CONFIG_HOME/wxapkg-dump` | `%APPDATA%\wxapkg-dump` |
| 数据 | 同上 | `$XDG_DATA_HOME/wxapkg-dump` | 同上 |
| 缓存 | `~/Library/Caches/wxapkg-dump` | `$XDG_CACHE_HOME/wxapkg-dump` | `%LOCALAPPDATA%\wxapkg-dump\cache` |
| 日志 | `<缓存>/logs` | 同左 | 同左 |

`WXAPKG_HOME` 覆盖以上全部，此时各角色分别落在 `<WXAPKG_HOME>/config`、`/data`、`/cache`。这个开关是给测试和便携安装用的。

### 配置目录内容

远程下载的账号与缓存由 `wxapkg-remote` 自管，根目录是 `<数据目录>/wxapkg-dump/remote/`。
**只记住一个账号**：`account.json` 存当前账号，新扫码直接覆盖；`删除账号` 清空以下全部内容。

| 路径 | 内容 | 可删除 |
|---|---|---|
| `remote/account.json` | 当前账号与会话凭证（schema 版本化，`0600`） | 否，删了要重新扫码 |
| `remote/pending/*.json` | 等待扫码的登录会话，重启后可续轮询 | 是 |
| `remote/metadata.json` | 小程序名称 / 图标 URL 缓存，按原始 ID 索引 | 是，会自动重建 |
| `remote/icons/` | 下载后的小程序图标，文件名为 URL 的 SHA-256 | 是 |
| `remote/.legacy-imported` | 旧库一次性导入的标记，删除账号后旧库不再复活 | 是 |

### 实际文件清单

整个配置根下**只有 6 个真实配置/数据文件**，其余全部是图标缓存（`.bin`，可整目录删除，
会自动重新下载）：

**用户视角只有 2 个真数据文件**：`settings.json`（界面偏好）和 `remote/account.json`
（登录账号）。其余全是缓存，删除后自动重建：

| 文件 | 类型 | 删除后果 |
|---|---|---|
| `settings.json` | 偏好 | 回默认值 |
| `remote/account.json` | **登录凭证** | 需重新扫码 |
| `remote/metadata.json` + `remote/icons/` | 名称与头像缓存 | 列表补全一轮 |
| `remote/pending/*.json` | 扫码中的临时会话 | 重新发码 |
| `cache/last-scan.json` | 上次扫描快照 | 自动重建 |

旧的多账号 `accounts.json`（若存在）在首次打开时取最新一条收编为单账号，原文件改名为
`accounts.json.legacy` 留底。存储全部是纯文件，
项目里没有任何数据库依赖。

### 数据目录内容

`original_wxapkg/`（远程下载的容器归档）与 `<appid>/`（还原出的工程）——
见「还原产物约定」。服务端与 GUI 在调用方未指定输出目录时，以 `<workspace>` 为基目录。

### 还原产物约定（标准布局）

**每个小程序一个顶层目录，目录名就是 appid。** 同一输出根下可以并存多个程序，互不覆盖；
输入没有 appid（任意位置的单文件容器）时退化为容器文件名，形状保持一致。

本机还原（GUI 的“还原源码”、CLI `unpack --appid`）：GUI 默认落在
`<workspace>/<appid>/`；CLI 按 `-o` 指定的基目录加 `<appid>`。

```text
<输出目录>/
└── wx069ba97219f66d99/       ← 工程根（DevTools 直接导入）
    ├── app.js / app.json / app.wxss
    ├── project.config.json
    ├── sitemap.json
    ├── pages/ components/ …
    ├── _raw/                 ← 容器副本；app.json.full.json 也只在这里
    ├── _foreign/
    └── _unpack_report.json
```

远程下载与还原：

```text
<基目录>/
└── original_wxapkg/
    └── <appid>/
        └── wxapkg/
            ├── manifest.json
            ├── <appid>_<module>_t<type>.wxapkg
            └── unpack_<appid>/     ← 工程根（DevTools 导入这一层）
```

用户直接选文件/文件夹时，工程生成在所选位置旁边：单个文件 → `<文件所在目录>/unpack_<appid>/`；
文件夹里多个 appid → 每个 `<文件夹>/unpack_<appid>/`；没有 appid 的容器合并为
`<文件夹>/unpack_<文件夹名>/`。

- `-o` 只是**基目录**，最终落在哪一层由程序计算，实际路径在 `report.outDir` 里返回。
- 用户直接选文件/文件夹时按 appid 分组，**一个 appid 一个工程**，目录名统一加 `unpack_`
  前缀；路径和文件名都没有 appid 的容器按所在文件夹合并；单个文件取文件主名。
- 加密容器需要 appid（或显式密钥）才能解密；无 appid 的加密包会被跳过。
- 美化与否只改变工程内文件的内容（格式化 vs 压缩），不改变布局。
- `_raw/`、`_foreign/`、`_unpack_report.json` 不属于小程序：`packOptions.ignore` 已排除，
  `verify` 与打包流程也跳过它们。
- 工程根只保留开发者工具认识的文件：`app.json.full.json` 这类诊断副本放在 `_raw/`。

## 2. 环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `WXAPKG_HOME` | 覆盖所有工具目录 | 平台标准位置 |
| `WXAPKG_WORKSPACE` | 服务端默认输出根 | `<数据目录>/projects` |
| `WXAPKG_BIND` | 服务端监听地址 | `127.0.0.1:8788` |
| `WXAPKG_ADMIN_KEY` | 服务端管理密钥 | 无（非回环时强烈建议设置） |
| `WXAPKG_LOG` | 日志级别 | `info` |
| `WXAPKG_REMOTE_HOME` | 远程登录与账号的存储目录（`wxapkg-remote`） | `<数据目录>/wxapkg-dump/remote` |
| `WXAPKG_SERVER` | 桌面外壳查找服务端二进制的路径 | 同级目录 / `PATH` |
| `NO_COLOR` | 关闭彩色输出 | 未设置 |
| `RUST_LOG` | 覆盖 `tracing` 过滤器 | 见 `WXAPKG_LOG` |

规则：

- **所有环境变量都以 `WXAPKG_` 开头**。不要引入不带前缀的变量，也不要读取无关环境的变量（例如系统版本、语言、时区）——工具不需要它们。
- **`WXAPKG_REMOTE_HOME` 是远程功能的唯一隔离开关。** 会话凭证、待扫码登录、图标与元数据缓存全部落在这个根下。任何新增的、与账号状态相关的文件都必须放在这个根下，否则「只设一个变量就隔离干净」这条性质会被破坏——早期版本就是因此让一次隔离测试读到了宿主机上的真实账号。
- 远程下载的代理遵循惯例变量（`HTTPS_PROXY` / `https_proxy` / `ALL_PROXY` / `all_proxy`），不再引入项目私有的代理变量。
- 平台目录由 `directories::BaseDirs` 决定，不要自行拼接 `~/Library/...` 之类的路径。

## 3. 命名

### Crate 与二进制

| 单元 | 名称 | 说明 |
|---|---|---|
| 引擎 | `wxapkg-core` | 无外壳依赖 |
| 远程下载 | `wxapkg-remote` | 纯 Rust 协议栈（MMTLS / iLink / UAL），无外部库 |
| 命令行 | `wxapkg-cli` → `wxapkg-dump` | |
| API 服务 | `wxapkg-server`（库）→ `wxapkg-dump --server` | 与 CLI 同一二进制 |
| 桌面外壳 | `wxapkg-dump-gui`（目录 `crates/wxapkg-gui`） | 独立于工作区 |
| MCP | `wxapkg-cli` → `wxapkg-dump --mcp` | 不单独发布二进制 |

### JSON 字段

两套命名并存是**有意为之**，不要「统一」：

- `wxapkg-core` / `wxapkg-cli` / `wxapkg-server` / `wxapkg-remote` 面向用户的载荷使用 `camelCase`（`outDir`、`totalSize`、`subPackages`、`indexLen`）。
- `wxapkg-remote` 的账号与小程序字段沿用上游协议的 `snake_case`（`login_id`、`nick_name`、`last_used_at`），因为这些字段直接映射到持久化记录，改名会让缓存文件与上游失去对应关系。

### 错误码

`Error::code()` 返回稳定字符串，跨 CLI / HTTP / MCP 复用同一套：

| code | 含义 | CLI 退出码 | HTTP |
|---|---|---|---|
| `invalid_argument` | 参数非法 | 2 | 400 |
| `invalid_container` | 不是合法容器 | 2 | 400 |
| `missing_decryption_key` | 缺 `V1MMWX` 密钥 | 2 | 422 |
| `program_not_found` | 本机没有该小程序 | 2 | 400 |
| `not_logged_in` | 没有会话 | 4 | 401 |
| `session_expired` | 会话失效 | 4 | 401 |
| `upstream_rejected` | 上游业务拒绝 | 5 | 502 |
| `network` / `protocol` | 网络 / 协议失败 | 5 | 502 |
| `unknown_account` | 账号不存在 | 2 | 404 |
| `no_data_directory` | 平台没有数据目录 | 3 | 500 |
| `io_error` / `storage_corrupt` | 运行时失败 | 3 | 500 |
| `internal_error` | 未分类 | 3 | 500 |

新增错误时必须同时给出 `code`、面向用户的 `message`，以及（当用户能自行处理时）`hint`。`hint` 会被原样展示，写成可执行的下一步。

### 输出信封

所有面向调用方的结果都是这两种形状之一，没有例外：

```json
{ "ok": true,  "command": "unpack", "version": "1.0.0", "protocol": 1, "data": { } }
{ "ok": false, "command": "unpack", "version": "1.0.0", "protocol": 1,
  "error": { "code": "program_not_found", "message": "…", "hint": "…" } }
```

`command` 用子命令名，空格分隔层级（例如 `download`）。人类可读输出走 stderr 或表格，**绝不**混进 stdout 的 JSON。

### 日志

`tracing` 目标统一为 `wxapkg-dump::<模块>`：

| target | 内容 |
|---|---|
| `wxapkg-dump::discover` | 目录扫描、不可读路径 |
| `wxapkg-dump::pipeline` | 容器采集 |
| `wxapkg-dump::remote` | 远程登录、会话状态、协议调用 |
| `wxapkg-dump::server` | 监听、关闭 |
| `wxapkg-dump::events` | SSE 事件总线 |
| `wxapkg-dump::paths` | 目录解析与迁移 |

日志写 stderr。**stdout 只承载协议输出**（CLI 的 JSON / MCP 的 JSON-RPC）——这是硬约束，否则管道会损坏。

## 4. 缓存策略

| 缓存 | 失效方式 |
|---|---|
| 小程序元数据（名称、图标） | 缓存记录不完整即时重取；完整记录 7 天后刷新 |
| 图标文件 | 以**原始 URL** 的 SHA-256 命名，内容不可变，永不过期。因此把 `http://` 升级为 `https://` 会改变缓存键，旧文件自然失效而不是被错误复用——这是有意的取舍 |
| 账号列表 | 以 `wxapkg-remote` 的本地库为准；上游会话的有效性在使用时验证，失效账号标记 `stale` 而不是删除 |
| 容器解析 | 不缓存，每次重新解析（包很小，解析很快） |

写缓存的纪律：**先写临时文件再 rename**，避免进程中断留下半个 JSON；账号文件额外 `0600`、目录 `0700`（会话即凭证）。读取失败一律回退到默认值而不是报错——缓存损坏不该让工具不可用（损坏的 `account.json` 会被移到 `account.corrupt-<时间戳>` 保留证据后从无账号开始）。

## 4.1 远程会话与账号

- 会话凭证（`app_session_key`、`auto_auth_key` 等）只存在 `remote/account.json`，十六进制编码，权限 `0600`；**绝不写进日志**，也不出现在任何 API 响应里。
- 一次扫码登录 = `pending/<id>.json`；进程重启后先恢复 pending 再继续轮询，用户不会因为关掉窗口而丢掉已显示的二维码。
- 上游判定会话失效（iLink `-14` / UAL `40030`）时映射为 `session_expired`，账号标记 `stale`；调用方先走一次 `sign_in_again` 静默续期（refresh token → login buffer → ManualAuth），失败才要求重新扫码。重新扫码后整条记录被覆盖。

## 5. 跨平台

- 路径拼接只用 `PathBuf`；输出目录名做安全检查（`is_safe_output_path`），拒绝 `<>:"|?*`、控制字符、结尾空格与点、以及任何 `..` 逃逸。
- 落盘前还要过 `paths::sanitize_segment`：容器条目名不是文件路径，`plugin-private://wx…/components/x` 这类虚拟方案在 Windows 上会以 `ERROR_INVALID_NAME` 直接失败。规则在每个平台都执行（`:` → `_`、保留设备名加前缀、去掉结尾点与空格），所以三个系统还原出同一棵树；`..` 变成 `_`，无法逃出输出目录。
- Windows 上写文件用 verbatim 前缀（`\\?\`），否则 appid + 分包路径很容易越过 `MAX_PATH`。
- 换行统一写 `\n`，读入时容忍 `\r\n`。
- 不假设大小写敏感，也不假设大小写不敏感；同名不同大小写的条目按原样保留。
- 时间戳一律 Unix 秒（`i64`），展示层负责格式化。
- 本地化文本（例如本机微信目录名）只在 `discover` 模块出现，其他模块不得硬编码平台路径。

## 6. 代码规范

- 注释与文档用英文，面向用户的帮助与文档可以中英双语。
- 注释解释**为什么**；不复述代码。反例：`// 遍历数组`。
- 每个 `pub` 项都要有文档注释（`#![warn(missing_docs)]`）。
- 不引入未使用的字段、常量、函数；`cargo clippy --workspace --all-targets -- -D warnings` 必须在 CI 通过。
- 新增第三方依赖前先检查 `[workspace.dependencies]`，能复用就复用。
- 字符串切片必须落在字符边界上；本项目处理的文本以中文为主，任何 `&s[a..b]` 都要能证明 `a`、`b` 是边界。`verify` 模块里有对应的回归测试。

## 7. 测试与门禁

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

除此之外必须跑真机语料：

```bash
wxapkg-dump scan --json                                   # 本机有哪些小程序
wxapkg-dump unpack --appid <appid> -o /tmp/x --beautify   # 解密 + 解包 + 美化
wxapkg-dump verify /tmp/x --node                          # 结构 + 真实 JS 语法
```

`verify --node` 是「能否被开发者工具编译」的可自动化代理：结构检查覆盖 `app.json` 引用、组件解析、`require` 解析、定界符平衡；`node --check` 覆盖真实的 JavaScript 语法。两者都通过，剩下的失败几乎只可能来自插件的运行时依赖。

## 8. 语言（i18n）

### 选择顺序

`--lang` > `WXAPKG_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG` > 兜底。

规则：**任何 `zh` 开头的标签都用中文**（`zh_CN`、`zh-Hans-CN`、`zh_TW`、`zh-HK`——中文读者看
简体远比看英文有用）；其它**合法的**标签用英文（`en_GB`、`de_DE`、`ja_JP`）；`C`、`POSIX`、
空串表示「未选择」，不算英文；**全都取不到时用中文**。

`LC_ALL` 优先于 `LC_MESSAGES` 优先于 `LANG`，这是 POSIX 的顺序，也是所有命令行工具的惯例。

### 目录表

翻译条目集中在 `wxapkg-core::i18n`，三个面共用。每个条目是 `match` 里的一个 `(中文, 英文)` 对，
因此**少写一种语言无法编译**——这比事后写测试检查更早地拦住错误。条目之间用命名占位符
（`{appid}`、`{path}`），不用位置参数：位置参数在语序不同的语言里会错位。

取不到翻译时保留原文占位符并记 `warn`，不返回空串。消息里出现 `{path}` 是一个一眼能看出的 bug，
而「读取  失败」看起来像一个真实的诡异故障。

### 不翻译的三类

- **错误码**（`program_not_found`…）。机器契约，跨语言必须逐字相同。
- **JSON 字段名**。同上。
- **日志**。面向开发者与 issue 报告，单一语言比双语好搜索。

### 动态片段

错误的**主句**全部来自目录表。少数变体携带解析器产生的技术细节（例如容器结构违规的具体原因），
这些细节保持英文，并在两种语言里都用 `（详情：…）` / `(detail: …)` 标出，以免读起来像翻译漏了。

`ProgramNotFound` 是例外而非样板：它只携带 `appid` 与可选的 `version`，整句由目录表在目标语言下
生成。**新增错误时应照此办理**——让错误携带数据，不要携带成句的英文，否则它会被拼进另一种语言的
句子里，产出中英混杂的消息。

### 现状

已覆盖命令行、HTTP API 与桌面程序（三者共用同一份目录表与同一套兜底规则）。桌面程序的界面
文案目前为中文，是已知缺口。
