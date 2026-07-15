# CLI Reference

`codexctl --help` is the canonical option list. This page describes the main workflows.

## Dashboard and output

```bash
codexctl
codexctl --demo
codexctl --list
codexctl --json
codexctl --watch
codexctl --headless --json
```

Use status, focus, project, and search filters to narrow the dashboard. Session controls can focus a terminal, send input, approve a prompt, compact context, launch a session, or terminate one.

## Brain

```bash
codexctl --brain
codexctl --brain --auto-run
codexctl --brain --url <endpoint> --brain-model <model>
codexctl --brain-query --tool Bash --tool-input "cargo test"
codexctl --mode on|off|auto|status
```

Advisory mode leaves execution under operator control. `--auto-run` permits automatic high-confidence actions. The immediate action set is approve, deny, send, terminate, route, and spawn.

## Learning and review

```bash
codexctl --brain-review [list]
codexctl --brain-mark-canonical <decision-id>
codexctl --brain-stats <report>
codexctl --brain-outcomes
codexctl --brain-baseline [--top N]
codexctl --insights [on|off|status]
codexctl --brain-garden [--apply]
codexctl --brain-briefing --project <name>
codexctl --autopsy [--session <id>]
```

Hook-facing outcome flags such as `--record-outcome` and `--reap-outcomes` feed the same local learning store.

## Setup and diagnostics

```bash
codexctl init
codexctl init --check
codexctl init --upgrade
codexctl init --remove
codexctl init --purge
codexctl doctor [--json]
codexctl completions <shell>
codexctl man
```

`init --upgrade` refreshes hooks and the onboarding marker without touching legacy state. `init --purge` is the explicit destructive path.

## Configuration compatibility

Legacy relay, hive, idle-task, and external-agent sections produce warnings and have no runtime effect. codexctl exposes no durable queue, dependency executor, distributed peer transport, or embedded project tracker.

## External coordination

Beads can track durable tasks, dependencies, claims, blockers, gates, and handoffs outside codexctl. It is an optional companion, not a linked library or background service.
