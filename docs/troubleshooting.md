# Troubleshooting

Start with:

```bash
codexctl doctor
```

It checks the binary on `PATH`, Codex hooks, brain endpoint, session discovery, and terminal integration. Advisories do not make the command fail.

## No sessions appear

Confirm a Codex session is running and that rollout files exist under `~/.codex/sessions/`. Run `codexctl --list` to separate discovery problems from terminal rendering problems.

## The brain is unavailable

Check the configured endpoint directly and verify the model name. For Ollama:

```bash
curl http://localhost:11434/api/tags
ollama list
```

Brain support is optional; the dashboard and deterministic rules still work without it.

## Non-loopback privacy advisory

This warning means the configured brain host is not `localhost`, `127.0.0.1`, or `::1`. Transcript context may leave the machine. Use a loopback endpoint or confirm the remote provider's data handling before enabling the brain.

## Legacy configuration warnings

`[relay]`, `[hive]`, `[idle]`, `[agents.*]`, and `lifecycle.retention_days` are no longer supported. Remove those entries when convenient. The warning is informational and does not delete legacy data.

## Upgrade or removal

`codexctl init --upgrade` refreshes hooks and preserves `~/.codexctl`. `codexctl init --remove` removes managed hooks but keeps data. `codexctl init --purge` deletes brain data and legacy codexctl state after confirmation.

## Terminal input or focus fails

Run `codexctl --doctor` for the legacy terminal-specific report and compare your terminal with the [support matrix](terminal-support.md). tmux and native terminal APIs may need to be enabled in the terminal itself.
