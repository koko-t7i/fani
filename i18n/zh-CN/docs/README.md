# 文档地图

使用此页面选择合适的 fani 文档。根目录中的 [`README.md`](../README.md) 是安装和首次运行的指南；它有意不重复所有配置、运维、架构或发布细节。

## 用户指南

- [`README.md`](../README.md)：产品概述、安装、五分钟快速入门、核心命令、退出代码和安全摘要。
- [`best-practices.md`](best-practices.md)：分阶段发布、提供商和凭据处理、仓库布局、日常操作、人工编辑、发布、调度和 CI。
- [`examples/fani.toml`](../../../examples/fani.toml)：完整的带注释配置参考。请在入门配置正常工作后复制并按需调整。
- [`systemd/`](../../../systemd/)：用于无人值守同步的用户服务和定时器模板。

## 维护者指南

- [`release.md`](release.md)：发布归档契约、验证与安装、可复现性范围以及打标签前检查。
- [`research/2026-rust-cli-stack.md`](research/2026-rust-cli-stack.md)：记录了支持原生实现的技术研究。

## 活动架构

- [`architecture/adr-0001-native-single-authority.md`](architecture/adr-0001-native-single-authority.md)：关于原生 Rust 引擎和单一 SQLite 权威数据源的已接受决策。
- [`architecture/native-i18n.md`](architecture/native-i18n.md)：当前 fani 0.3 的产品与实现契约。

当用户指南与架构文档的详细程度不同时，架构契约定义系统行为，而用户指南则定义推荐的操作路径。

## 历史记录

以下文档描述了已被取代的预发布设计。保留这些文档是为了记录决策历史，不得将其用作设置或兼容性说明：

- [`architecture/compatibility-baseline.md`](architecture/compatibility-baseline.md)
- [`architecture/rust-sqlite-rewrite.md`](architecture/rust-sqlite-rewrite.md)
