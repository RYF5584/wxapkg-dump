# wxapkg-dump-gui

`wxapkg-dump-gui` 是 wxapkg-dump 的 Tauri 桌面程序。`wxapkg-dump --gui` 会找到并启动它。

## 设计取舍

**直接调用引擎，不经过 HTTP。** 每个窗口命令都是对 `wxapkg-core` / `wxapkg-remote` 的薄适配：
扫描、结构、还原、原样导出、美化、校验，以及远程下载的登录、账号、最近使用、信息与下载。
窗口自身的功能不需要访问密钥，也不打开端口。

界面资源以 `crates/wxapkg-server/webui` 为唯一来源（HTML/CSS/JS，无框架、无构建步骤）。
`build.rs` 在每次构建前把 `app.js`、`index.html`、`styles.css` 同步到本 crate 的 `webui/`，
再由 `tauri.conf.json` 的 `frontendDist` 打包。Tauri CLI 会在运行 `build.rs` 之前检查该目录，
所以 `webui/` 作为快照提交，CI 会校验它与 `wxapkg-server/webui` 逐字节一致。
窗口只通过 `invoke` 与 Rust 通信，日志经 `wxapkg-log` 事件流进抽屉。

## 唯一的监听器由操作者决定

「API 服务」面板调用 `server_start` / `server_stop`，在进程内托管一个公开的 HTTP 服务
（`wxapkg-server`），可自选地址、端口与访问密钥。它默认关闭，与窗口自身的功能无关：

- `wxapkg-dump-gui`：不监听任何端口；
- `wxapkg-dump --gui --server --bind … [--admin-key …]`：启动窗口时一并拉起公开服务，
  地址与密钥经 `WXAPKG_PUBLIC_API` / `WXAPKG_PUBLIC_KEY` 交给外壳。

## 构建

```bash
cd crates/wxapkg-gui
cargo build --release    # 产出 target/release/wxapkg-dump-gui
```

需要安装包（dmg / msi / AppImage）时用 `cargo tauri build`（先
`cargo install tauri-cli --version "^2"`）。本 crate 故意留在 Cargo 工作区之外，
`cargo build --workspace` 不会拉起 Tauri 工具链。

## 与 CLI 的关系

`wxapkg-dump --gui` 按以下顺序查找外壳；都找不到时报错并说明如何补上：

| 查找顺序 | 位置 |
|---|---|
| 1 | `WXAPKG_GUI` 环境变量指定的文件 |
| 2 | 与 `wxapkg-dump` 同级的 `wxapkg-dump-gui`（Windows 为 `.exe`） |
| 3 | 同级的 `wxapkg-dump-gui.app/Contents/MacOS/wxapkg-dump-gui` |
| 4 | 兼容旧名：`wxapkg-gui`、`wxapkg-dump.app` |
