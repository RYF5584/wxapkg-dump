# 贡献指南

## 开发环境

- Rust 1.90+（CLI 最低 1.88）（`rustup toolchain install stable`）

## 仓库约定

- 这是应用/二进制项目，`Cargo.lock` 与 `crates/wxapkg-gui/Cargo.lock` 都提交，保证构建可复现。
- 不提交 `target/`、`dist/`、DMG/ZIP/EXE 等产物；二进制只通过 GitHub Releases 发布。
- `crates/wxapkg-gui/webui/` 是 `crates/wxapkg-server/webui/` 的提交快照，`build.rs` 会同步，CI 会校验。

## 提交前检查

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 代码规范

- 注释、文档、提交信息使用英文；面向用户的文档与 CLI 帮助可以中英双语。
- 注释解释**为什么**，不复述代码在做什么。
- 每个公开项都要有文档注释；错误类型要给出稳定的 `code()`，供 CLI / HTTP / MCP 复用。
- 新增能力时同步更新 `docs/` 与 README 的能力表。

## 结构约定

- `wxapkg-core` 不依赖任何外壳；外壳只调用 `pipeline` 或各模块的公开 API，不复制业务逻辑。
- 所有面向用户的输出走统一信封：成功 `{ok:true,command,version,data}`，失败 `{ok:false,command,version,error:{code,message,hint?}}`。
- 新增格式支持时，先在 `container.rs` 里加分发分支，再补 `#[test]`。

## 不接受的改动

- 提交任何真实的会话凭据、cookie 或 token。
- 在 `wxapkg-core` 里引入对微信客户端、开发者工具或 JS 引擎的运行时依赖。
- 引入与现有纯 Rust 实现重复的私有二进制桥接层。

## 分支策略

- `main` —— 生产分支。只接受来自 `develop` 的合并，每个提交都应当是可发布的。
- `develop` —— 集成分支。日常开发与 pull request 的目标分支。
- 特性分支从 `develop` 切出，命名为 `feat/<简短描述>` 或 `fix/<简短描述>`，通过 pull request 合回。

发版时把 `develop` 合并到 `main` 并打标签；发布工作流据标签产出各平台产物。

## 本机与 CI 的一致性

`rust-toolchain.toml` 固定了工具链，`rustfmt.toml` 只使用 stable 支持的选项，因此本机跑通的检查
在 CI 上同样会通过。合并前请至少执行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```
