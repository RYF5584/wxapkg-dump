# 命令行参考

`wxapkg-dump` 有三种运行方式，同一个可执行文件：

| 形式 | 作用 |
|---|---|
| `wxapkg-dump <子命令>` | 命令行，见下文各节 |
| `wxapkg-dump --gui` | 启动桌面程序；窗口直接调用引擎，不需要密钥，也不监听端口 |
| `wxapkg-dump --server` | HTTP API 与 `/docs` 接口文档，行为见 [api.md](api.md) |
| `wxapkg-dump --mcp` | MCP stdio 服务器；不再单独发布 `wxapkg-mcp` 二进制 |

`--gui` 与 `--server` 在子命令解析之前拦截，因此可以带自己的参数（`--bind`、`--workspace`、
`--admin-key`、`--allow-remote`），组合写法 `wxapkg-dump --gui --server --bind … --admin-key …` 会
打开窗口并同时把公开 API 拉起来。`--gui` 会依次查找 `WXAPKG_GUI`、与 `wxapkg-dump` 同级的
`wxapkg-dump-gui`（Windows 为 `.exe`）和 `wxapkg-dump-gui.app`；找不到时报错并说明如何补上。

## 全局选项

| 选项 | 说明 |
|---|---|
| `--json` | 输出机器可读 JSON 信封而非表格 |
| `-q, --quiet` | 只输出错误 |
| `-v, --verbose` | 提高日志级别（可叠加 `-vv`） |
| `--no-color` | 关闭彩色输出（`NO_COLOR` 亦生效） |

## 退出码

| 码 | 含义 |
|---|---|
| `0` | 成功 |
| `1` | 完成但有警告（仅 `--strict` 时返回） |
| `2` | 参数或输入非法 |
| `3` | 运行时错误 |
| `4` | 未登录或会话过期 |
| `5` | 上游拒绝 |

## `wxapkg-dump scan`

位置参数与 `--path` 等价：`scan .`、`scan D:\\cache`。

| 选项 | 默认 | 说明 |
|---|---|---|
| `--path <DIR>` | 平台默认根 | 追加搜索根，可重复 |
| `--appid <wx…>` | — | 只显示指定小程序 |
| `--limit <N>` | — | 最多显示多少个安装 |
| `--packages` | 关 | 额外列出每个安装的包 |

## `wxapkg-dump info <输入…>`

| 选项 | 说明 |
|---|---|
| `--wxid <wx…>` | `V1MMWX` 加密包的解密密钥，默认从路径自动识别 |
| `--entries` | 列出容器内全部条目 |

## `wxapkg-dump unpack <输入…> -o <目录>`

输入是 `.wxapkg` / `.zip` / 目录的任意组合，也可以用 `--appid` 自动定位。

输出目录由输入形式决定，实际路径以 `--json` 返回的 `outDir` 为准，终端也会打印：

| 输入 | 结果 |
|---|---|
| 单个容器文件，无 `-o` | `<文件所在目录>/unpack_<appid>/`；没有 appid 时用文件主名 |
| 一个文件夹，无 `-o` | 递归收集容器，**按 appid 分组**：`<文件夹>/unpack_<appid>/` |
| 文件夹里没有 appid | 同目录下的容器合并为 `<文件夹>/unpack_<文件夹名>/` |
| 多选文件，无 `-o` | 与文件夹相同，按 appid / 所在目录分组 |
| `--appid` | `<out 或当前目录>/<appid>/` |
| 显式输入 + `-o <目录>` | 上述 `unpack_*` 统一放到该基目录下 |
| 加密容器 | 需要 `--appid` 或 `--wxid`；没有密钥的加密包会被跳过并报错 |

分组只影响落盘位置：同一 appid 的主包、分包、插件仍合并成一个工程；不同 appid 永远不会
被拼进同一个工程。

| 选项 | 默认 | 说明 |
|---|---|---|
| `-o, --out <目录>` | 当前目录 | 输出基目录；不传时写到当前目录 |
| `--appid <wx…>` | — | 从本机微信缓存定位 |
| `--version <N>` | 最新 | 配合 `--appid` 选择版本目录 |
| `--wxid <wx…>` | 自动 | 解密密钥 |
| `--main <文件名>` | 自动 | 显式指定主包 |
| `--raw <目录>` | — | 不还原，改为把容器条目原样导出到该目录 |
| `--auto-inflate` | 关 | 配合 `--raw`：对疑似 zlib 压缩的条目尝试解压（带明文本签名校验） |
| `--clean` | 关 | 输出前清空输出目录 |
| `--no-raw` | — | 不保留 `_raw/` |
| `--no-wxml` | — | 跳过 WXML 还原（最快） |
| `--no-require-rewrite` | — | 保留原始 `require()` 写法 |
| `--no-prune` | — | 不裁剪无源码页面 / 分包 |
| `--no-synthesize` | — | 不给缺失 WXML 合成壳 |
| `--no-plugin-clean` | — | 不清理无源码插件与 `plugin://` 引用 |
| `--no-auto-fix` | — | 以上四项全关（忠实模式） |
| `--plugins <keep\|placeholder\|strip>` | `keep` | `app.json` 插件声明处理方式 |
| `--wxss-suffix <S>` | 空 | 给样式选择器加作用域后缀 |
| `--beautify` | 关 | 对还原产物做美化 |
| `--indent <N>`, `--use-tabs` | `2`, 关 | 美化缩进 |
| `--strict` | 关 | 有警告即退出码 1 |
| `--max-warnings <N>` | `20` | 终端最多显示多少条警告，全量始终写入 `_unpack_report.json` |

## `wxapkg-dump beautify <目录>`

| 选项 | 默认 |
|---|---|
| `--indent <N>` | `2` |
| `--use-tabs` | 关 |
| `--max-width <N>` | `110` |
| `--collapse-lines` | 关（默认保留源码原有换行） |


## `wxapkg-dump doctor`

打印版本、目标平台、远程引擎与配置目录、各搜索根源状态。排障第一步。

## `wxapkg-dump completions`

生成 shell 补全脚本，`source` 或放入对应补全目录即可：

```bash
wxapkg-dump completions zsh > "${fpath[1]}/_wxapkg-dump"   # zsh
wxapkg-dump completions bash > ~/.local/share/bash-completion/completions/wxapkg-dump
```

支持 `bash`、`zsh`、`fish`、`powershell`、`elvish`。

## 远程下载

只记住一个账号：新扫码直接覆盖旧账号。

| 命令 | 说明 |
|---|---|
| `wxapkg-dump account-status` | 当前账号（昵称 / 头像 / 在线状态） |
| `wxapkg-dump login` | 获取二维码并轮询扫码结果；确认后替换当前账号 |
| `wxapkg-dump recent` | 最近使用的小程序（含名称与图标缓存），按页读取 |
| `wxapkg-dump remote-info` | 小程序基本信息 |
| `wxapkg-dump download` | 下载主包与全部分包 |
| `wxapkg-dump account-delete` | 删除当前账号与其全部缓存（需 `--yes`） |

### `login`

无子命令 = 交互模式：渲染二维码并轮询到确认为止。

分离模式（RFC 8628 风格，AI/脚本/多机场景）：`get-qr` 发码即退，
`check-qr` 用返回的 `session` 随时轮询，两端可跨进程：

```bash
wxapkg-dump login get-qr        # → {session, qr_url, expires_in}
wxapkg-dump login check-qr --session <session>   # → waiting/scanned/confirmed/…
```

| 选项 | 适用 | 默认 | 说明 |
|---|---|---|---|
| `--wait <秒>` | 交互模式 | `180` | 展示二维码后轮询多久 |
| `--qr-out <文件>` | 交互模式 / `get-qr` | — | 同时写出 PNG 二维码 |
| `--no-terminal` | 交互模式 | 关 | 不把二维码打到终端 |
| `--session <key>` | `check-qr` | 必填 | `get-qr` 返回的会话键 |

`get-qr` 发码后立即退出、不在终端渲染二维码，所以没有“只取内容不等待”的独立开关。

### `recent`

| 选项 | 默认 | 说明 |
|---|---|---|
| `--page <N>` | `1` | 页码（1 起） |
| `--size <N>` | `50` | 每页条数（1–100） |
| `--starred` | 关 | 只看星标记录 |

### `remote-info`

接受与 `download` 相同的目标选择：位置参数 `wx…` / `gh_…`、`--appid` 或 `--username`。

### `download`

| 选项 | 默认 | 说明 |
|---|---|---|
| `<ID>` | — | 位置参数：`wx…` 按 appid、`gh_…` 按原始 ID；与下面两个旗标互斥 |
| `--appid <wx…>` / `--username <gh_…@app>` | — | 至少给一个目标（或上面的位置参数） |
| `-o, --out <目录>` | 必填 | 下载目录 |
| `--version-type <N>` | `0` | `0` 正式、`1` 体验、`2` 开发、`999` 预览 |
| `--version <N>` | `0` | 指定版本号，`0` 为最新 |
| `--unpack` | 关 | 下载后立刻用本地引擎还原 |
| `--beautify` | 关 | 配合 `--unpack` 做美化 |
| `--all-packages` | 关 | 连资源全量包（type 0/22 等）一起下载；体积可能翻数倍 |
| `--concurrency <N>` | `3` | 并行下载数（1–10） |

远程下载的落盘结构：`<out>/original_wxapkg/<appid>/wxapkg/` 存放容器与 manifest，
`--unpack` 时工程还原到同目录下的 `unpack_<appid>/`。

**资源包是什么**：type 4/23 是代码包（主包/分包），DevTools 打开、编译、还原只依赖它们。
type 0/22 是资源全量包，实测会包含同一程序的 `app-service.js`、页面，以及按需加载的
`chunk_*.appservice.js` / `chunk_*.webview.js`（某样本：代码包 56.0 MiB，全量 154.5 MiB，
2.76 倍）。不下载它们不影响打开与还原；只有需要分析动态 chunk / 静态资源时才加
`--all-packages`。type 8/15/28 是 0 字节的配置桩。

## 环境变量

| 变量 | 说明 |
|---|---|
| `WXAPKG_HOME` | 覆盖配置目录 |
| `WXAPKG_WORKSPACE` | 覆盖服务器默认输出目录 |
| `WXAPKG_REMOTE_HOME` | 远程账号存储目录（会话、图标、元数据缓存的唯一隔离开关） |
| `RUST_LOG` | 覆盖日志级别 |

## 语言

全部子命令接受 `--lang`，也可用 `WXAPKG_LANG` 环境变量或系统区域设置：

```bash
wxapkg-dump --lang en-US unpack --appid wx… -o /tmp/out
WXAPKG_LANG=en wxapkg-dump verify /tmp/out
```

优先级为 `--lang` > `WXAPKG_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG` > 兜底。任何 `zh` 开头的
标签用中文，其它合法标签用英文，**取不到时用中文**。错误码不随语言变化，只有 `message` 与 `hint` 会。
命令名、选项名与 `--help` 文本固定为英文，保证脚本与 shell 补全稳定；错误 `message` / `hint`
跟随上述规则。详见 [conventions.md](conventions.md#8-语言i18n)。

## `verify`

校验一个已经还原的工程，回答的是「它能不能被开发者工具编译」这个问题。

```bash
wxapkg-dump verify /tmp/out --json
wxapkg-dump verify /tmp/out --node        # 追加真实的 JavaScript 语法检查
```

结构检查（始终执行）：

- `app.json` 的声明与磁盘实况是否一致：页面、分包、组件、插件的引用是否都能解析到文件。
- 每个页面的 `.js` 必须存在（缺失记为错误），`.wxml` 缺失记为警告；页面的 `.json` 在微信里
  本身是可选的，缺失不算缺陷。
- `require()` / `import` 的目标是否可解析，解析不到时给出相对路径。
- `usingComponents` 的每个引用是否落在文件上。
- JS / WXSS / WXML 的定界符是否平衡。

`--node` 额外做一次真实的语法解析：把每个 JavaScript 文件交给 Node 的 `vm.Script` 编译一次，
不执行。这能抓到定界符平衡检查抓不到的语法错误——例如美化过程引入的非法表达式。

退出码：全部通过为 `0`，有 `error` 级问题为 `2`；仅有 `warn` 时为 `0`，加 `--strict` 则变为 `1`。

**它不能替代什么**：插件的运行时依赖无法离线复现，那类失败只能在开发者工具里看到。`verify` 覆盖
的是所有可以离线判定的问题。
