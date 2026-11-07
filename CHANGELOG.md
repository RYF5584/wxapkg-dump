# 更新日志

本文件记录所有值得注意的变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [1.0.0] - 2026-11-08

### 新增

- 本机小程序扫描、`V1MMWX` 解密、WXML/WXSS/JS 还原、工程重建、美化与校验。
- 授权远程下载：扫码登录、最近使用、按 appid 或原始 ID 下载主包/分包/资源包。
- 桌面 GUI：本机/远程列表、详情、批量还原、下载中心、日志、设置、中英双语。
- 单一 CLI 承载 `--server`（HTTP API + Swagger）与 `--mcp`（9 个 MCP 工具），不再发布单独的 MCP 二进制。
- 统一 JSON 信封、稳定错误码、退出码与 `--lang` / `WXAPKG_LANG`。
- GitHub Actions：CI（格式化、clippy、测试、文档）与 Release（macOS arm64/x64、Windows x64、Linux x64）。

### 变更

- 输出统一按 appid 分目录；远程下载保留 `original_wxapkg/<appid>/wxapkg/` 与 `manifest.json`。
- 计算与文档全部使用简洁英文注释；GUI 文案完整中英双语。
- 发布资产使用版本无关的稳定文件名，README 直接链接 GitHub Releases 的 `latest`。
- Rust 最低支持版本 1.88；提交锁定文件以保证可复现构建。

### 修复

- WXML 模板 `data`、裸 `wx:*` 指令、重复 `wx:else`、属性对象字面量/展开、UTF-8 截断。
- Skyline 页面配置、页面 JSON `window` 扁平化、WXSS 选择器根块、非法 renderer。
- 分包/插件引用、`require()` 解析、校验退出码与 `--strict` 行为。
- GUI 本机/远程详情 footer 串用、批量导出性能、远程页自动加载最近使用。
- 环境探测不再枚举微信目录，避免受保护目录导致接口阻塞。
- 干净 checkout 下 Tauri 前端资源检查失败。
