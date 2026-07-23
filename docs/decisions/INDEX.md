# Architecture Decision Records

| ADR | Status | Decision |
| --- | --- | --- |
| [ADR-0001](ADR-0001-lifecycle-hooks-as-status-evidence.md) | Accepted | Treat Codex lifecycle hooks as bounded status evidence while preserving Bash-only brain authorization. |
| [ADR-0002](ADR-0002-coding-brain-product-boundary.md) | Accepted | Make Brain the sole Coding Brain TUI, remove session management, and adopt the `coding-brain` public namespace. |
| [ADR-0003](ADR-0003-fail-safe-hook-and-learning-persistence.md) | Accepted | Separate model proposals, committed hook decisions, delivery, and execution while publishing learning state atomically. |
| [ADR-0004](ADR-0004-provider-aware-guards-and-terminal-actuation.md) | Accepted | Use provider-qualified identity, structured permission and recovery hooks, and guarded terminal fallback for process-only, manual, and unsupported prompts. |
