# Rust + SQLite 重写设计（已弃用）

fani 0.2 的设计保留了一个外部 Python i18n 技能和一个兼容性状态边界。该项目尚未发布，而这一架构已于 2026 年 8 月 30 日被破坏性原生实现所取代。

本文档仅作为历史背景保留，不得用作实施或运维指南。有关已采纳的替代决策，请参阅 [`ADR-0001`](adr-0001-native-single-authority.md)；有关当前约定，请参阅 [`native-i18n.md`](native-i18n.md)：原生 Markdown 处理、单一 SQLite 权威数据源、隔离的类型化 Agent 执行、固定来源的 Git 发布，以及 GitHub 拉取请求协调，且不使用 Python、`uv`、外部技能、旧版 JSON 或兼容性迁移。
