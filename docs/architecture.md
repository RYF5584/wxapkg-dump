# 架构

## 分层

```
                    ┌───────────────┐  ┌───────────────┐  ┌──────────────┐  ┌────────────┐
                    │  wxapkg-cli   │  │ wxapkg-server │  │ wxapkg-gui   │  │ --mcp      │
                    │   (命令行)     │  │    (HTTP)     │  │  (Tauri)     │  │  (stdio)   │
                    └───────┬───────┘  └───────┬───────┘  └──────┬───────┘  └─────┬──────┘
                            └──────────────────┴─────────────────┴────────────────┘
                                                       │
                        ┌──────────────────────────────┴──────────────────────────────┐
                        │                       wxapkg-core                            │
                        │  container → crypto → js → decompile → project → beautify   │
                        └──────────────────────────────┬──────────────────────────────┘
                                                       │ 只有远程下载需要
                        ┌──────────────────────────────┴──────────────────────────────┐
                        │  wxapkg-remote：MMTLS / iLink / UAL，纯 Rust，无外部库      │
                        └─────────────────────────────────────────────────────────────┘
```

**铁律**：外壳不含业务逻辑。每个外壳只做三件事——解析参数、调用 `pipeline` 或模块 API、渲染结果。新增能力时先落在 `wxapkg-core`，再暴露给外壳。

**桌面程序不经过 HTTP。** `wxapkg-dump-gui`（目录 `crates/wxapkg-gui`）的每个命令都是对 `wxapkg-core` / `wxapkg-remote` 的薄适配，
窗口因此不需要密钥、不需要端口，也不会因为服务没启动而少一半功能。`wxapkg-server` 只服务外部
调用方（脚本、其它工具、AI）；窗口里的「API 服务」面板按需把它托管在进程内，是唯一可选的监听器。

## 数据流

```
.wxapkg ──container::Wxapkg::parse──▶ entries[]
                                        │
             ┌──────────────────────────┼───────────────────────────┐
             ▼                          ▼                           ▼
   decompile::bundle            decompile::wxml             decompile::wxss
   define() 切分 / JSON          $gwx 虚拟机 / WXML 序列化     setCssToHead 还原
             │                          │                           │
             └──────────────────────────┼───────────────────────────┘
                                        ▼
                              project::ProjectBuilder
                      角色识别 → 落盘 → app.json 重建 → 自动修复
                                        ▼
                              beautify（可选）→ 工程目录
```

## 关键设计决策

### 1. 容器 v0 与 v1 的 offset 语义不同

v0 的 `offset` 是**文件绝对偏移**，v1 是**相对段基址的偏移**（`enc_type=1` 指第一数据段，`0` 指第二段）。照抄任一实现都会在另一种包上翻车，所以 `container.rs` 按 `version` 分发到两套解析器。

### 2. 反编译不引入 JS 引擎

`wcc` 生成的模板渲染函数只用到一小撮语法（`var`/`if`/`try`/函数表达式/调用/成员访问/三元）。`js/` 里实现了一个字符级词法器与递归下降解析器，只覆盖这些形态；`decompile/wxml.rs` 解释这台微型虚拟机，把虚拟 DOM 序列化回 WXML。代价是解析器不理解完整 ECMAScript，收益是零运行时依赖、可交叉编译到任意平台。

### 3. 操作数是「带操作码的嵌套数组」

真实 `wcc` 产物形如：

```js
Z([3,'hello'])                       // 字符串字面量
Z([[7],[3,'b']])                     // 变量引用 b
Z([[2,'+'],[1,'height:'],[[7],[3,'f']]])   // 字符串拼接
Z([[6],[[7],[3,'tab']],[3,'b']])     // tab.b 成员访问
```

即**外层数组的第一个元素本身是一个只含操作码的数组**。实现时必须先判断 `items[0]` 是数字（简单操作码）还是数组（复合操作码），二者分支完全不同。这是移植时最容易出错的一点。

### 4. 同一模板的多个变体

一个包会同时包含 `chunk_N.appservice.js` 与 `chunk_N.webview.js`，两者对**同名操作数数组**给出的长度可以不同（新版本 `wcc` 会在中间插入额外操作数，后续下标整体位移）。把渲染函数和错误的那份数组配对，会让所有绑定表达式静默错位。

因此实现保留了每个组的**全部变体**，并在配对上取「长度不小于渲染函数引用到的最大下标」的**最小**变体；没有覆盖时退回最长的一份。同时 `wxml_units` 以质量分（未解析绑定数越少越好、内容越长越好）挑选最终落盘的版本。

### 5. 自动修复而非忠实复刻

导出工具常常只导出用户打开过的分包，插件源码也不在包里。忠实地搬运 `app.json` 会得到一个无法编译的工程。因此 `ProjectBuilder` 默认：

- 裁掉磁盘上没有源码的页面与分包，同步修 `entryPagePath` / `tabBar` / `preloadRule` / `workers`；
- 删掉无源码的插件声明，并清掉指向它的 `plugin://` 组件引用；
- 清掉指向不存在组件的 `usingComponents`；
- 给缺 WXML 的页面 / 组件合成 `<view />` 壳。

完整声明保留在 `app.json.full.json`，`--no-auto-fix` 可完全关闭。

### 6. 远程下载是纯 Rust 协议栈

`wxapkg-remote` 自带完整传输链：UAL 开放平台（换取 `login_buffer`）→ MMTLS 握手（P-256 ECDH + HKDF + AES-GCM 记录层）→ iLink 信封（HybridEcdh 双封、ManualAuth/AutoAuth）→ 业务 CGI（`WxaRuntimeTransfer` 包裹 recent / info / download）。这样：

- 不依赖任何共享库，交叉编译到 Windows / macOS / Linux 不需要目标机的工具链；
- 协议升级就是普通的 Rust 提交，可测试、可 review；
- 会话凭证留在本 crate 的存储里（`0600`），不经过任何第三方进程。

网络调用是同步阻塞的，外壳必须在阻塞线程池上调用（CLI 直接调用，server 用 `spawn_blocking`）。

## 目录约定

| 路径 | 内容 |
|---|---|
| `~/.config/wxapkg-dump`（macOS 为 `~/Library/Application Support/wxapkg-dump`） | 设置 |
| `<数据目录>/wxapkg-dump/remote/` | 远程账号、会话、待扫码登录、图标与元数据缓存 |
| `_raw/` | 还原时保留的原始编译产物，已在 `project.config.json` 中排除打包 |
| `_foreign/` | 判定为「另一个小程序」的容器，隔离存放 |
| `_unpack_report.json` | 包归属、还原统计、全部警告 |

## 扩展点

- **新容器格式**：在 `container::Wxapkg::parse` 的版本分发里加分支。
- **新反编译器产物**：在 `decompile/` 下加模块，并在 `project::ProjectBuilder::decompile_bundle` 里接上。
- **新外壳**：实现 `pipeline` 的调用即可复用全部能力与输出约定。
- **新远程 CGI**：在 `wxapkg-remote` 的 `pb.rs`（protobuf 生成）里补消息定义，再加一个传输封装；所有请求共用同一条 MMTLS / iLink 通道。
