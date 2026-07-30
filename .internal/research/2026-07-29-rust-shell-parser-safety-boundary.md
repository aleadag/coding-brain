# Research: Rust Shell Parser Boundary for Deterministic Safety

> **Date:** 2026-07-29
> **Bead:** codexctl-oq92
> **Status:** Complete

## Summary

The reopened `codexctl-msos` failures show that extending the hand-written shell lexer is not a sound path: adjacent valid Bash glob, redirection, brace, and arithmetic forms repeatedly cross its lexical assumptions. The best next step is a narrow acceptance spike of `brush-parser` 0.4.0 behind a project-owned adapter, with parser failures and unsupported AST forms mapped to native confirmation rather than model inference; Tree-sitter Bash is useful structural evidence but does not remove the need for word-level Bash interpretation.

## Key Findings

### `brush-parser` is the leading acceptance-spike candidate

> **Confidence:** high — current official API and release documentation were independently re-fetched and checked; local compilation/probing also succeeded.

`brush-parser` 0.4.0 is MIT-licensed, declares the same Rust 1.88 minimum supported by this repository, and exposes `Parser::parse_program` as a parser-to-AST API. Its AST structurally represents brace groups, subshells, I/O redirects, pipelines, arithmetic commands, and process substitutions; its companion word parser distinguishes text and quoting forms, parameter expansion, command substitution, escape sequences, and arithmetic expressions. [S1] [S2] [S3]

A local throwaway probe compiled `brush-parser` 0.4.0 under the repository's Nix environment and parsed the critical shapes without executing them:

- `rm>/dev/null -rf /` retained `rm` as the command and the adjacent redirect as an `IoRedirect`.
- process substitution and arithmetic compound commands became distinct AST forms.
- ANSI-C quoting and parameter expansion remained word syntax for the companion word parser.
- `/bin/r[]m]` remained a raw command word, so the adapter must additionally use or reproduce Brush's pattern classification rather than treating a parsed word as literal.
- `sh -c '...'` remained an outer command plus a string argument; nested-shell enforcement is not solved automatically.

This is evidence for a spike, not adoption. The spike must prove exact quote, brace, pattern, and source-span behavior against the complete policy corpus before production code depends on it.

### Tree-sitter Bash is structural, not sufficient by itself

> **Confidence:** medium — official grammar/runtime documentation supports the syntax and error-node facts; an independent check could not tie the full broad-policy conclusion to the exact 0.25.1 source tag.

`tree-sitter-bash` 0.25.1 exposes a Bash syntax tree with named redirects, command and process substitutions, arithmetic expansion, ANSI-C strings, grouping, pipelines, and source spans. Tree-sitter also exposes `has_error`, `is_error`, and `is_missing`; any use at a safety boundary must reject error recovery and missing nodes rather than accepting any returned tree. [S4] [S5] [S6]

The published grammar/node inventory has an `extglob_pattern` and numeric `brace_expression`, but does not expose general pathname globbing or comma-brace expansion as equivalent first-class semantic nodes. Bash performs brace, parameter, word-splitting, filename, and quote-removal phases after parsing, so an AST alone does not establish the executable or arguments ultimately invoked. [S7] [S8] The existing broad “recognize dangerous deletion while leaving unrelated Bash undecided” policy would therefore still require custom word-level interpretation for shapes such as `/bin/r[]m]` and `/{,}`.

Tree-sitter remains viable only for a much narrower policy that rejects nearly every dynamic or compound construct and permits a small literal allowlist. That would be a material product-policy change, not a drop-in fix for `codexctl-msos`.

### Parse uncertainty needs an explicit hook outcome

> **Confidence:** high — independently verified against the current evaluator, hook control flow, and provider behavior ADR.

`safety::evaluate` currently returns `Option<SafetyDeny>`. `None` proceeds through provider policy and then model inference, so it cannot also mean “the parser failed or encountered unsupported syntax.” The parser integration needs an explicit outcome such as:

```rust
enum SafetyEvaluation {
    Deny(SafetyDeny),
    NoDeterministicDecision,
    Indeterminate(ShellAnalysisError),
}
```

The permission hook should preserve deterministic and provider-policy denies, map `Indeterminate` to the existing native-confirmation behavior before inference, and send only `NoDeterministicDecision` through the current inference flow. This matches ADR-0004's rule that malformed or unsupported input leaves the provider's native behavior in control. [S9] [S10]

### A parser does not remove the policy boundary

> **Confidence:** high — Bash's official execution model and both parser APIs agree on the parse-versus-expansion separation.

The adapter must not execute shell expansion, inspect the live filesystem to resolve globs, look up executables, or evaluate the environment. Bash parses before it performs expansions and executes the command; `sh -c`, `bash -c`, `eval`, dynamically produced command names, aliases, functions, and runtime variable contents remain outside a top-level syntax parser's guarantees. [S8] [S11]

`codexctl-dchq` should continue to own nested command-string enforcement. The `codexctl-msos` parser migration should preserve that explicit boundary while ensuring unsupported syntax cannot silently fall through to inference.

## Comparisons

| Criterion | `brush-parser` 0.4.0 | `tree-sitter-bash` 0.25.1 | `conch-parser` 0.1.1 | `yash-syntax` 0.23.x |
|-----------|----------------------|---------------------------|----------------------|----------------------|
| Bash-oriented typed AST | Strong; redirects, groups, process substitution, word parser | Strong syntax tree; weaker word-expansion semantics | POSIX-oriented and missing required Bash breadth | Strong POSIX word/quote AST |
| Parse-error contract | `Result<Program, ParseError>` | Recovery tree; caller must reject error/missing nodes | Runtime-free parser result | Strong structured syntax errors |
| Repository Rust 1.88 | Exact match | Compatible runtime observed | Compatible but stale | Declares Rust 1.96 |
| License | MIT | MIT | MIT/Apache-2.0 | GPL-3.0-or-later |
| Maintenance signal | Current 0.4.0 release and active parser work | Current 0.25.1 release | Last release 2019-05-15 | Current, but incompatible constraints |
| Fit | **Acceptance spike** | Reject as sole evaluator | Reject | Reference only unless license/MSRV change |

`bashrs` was also considered and rejected because its transpilation and analysis surface is much broader than the parser-only boundary required here.

## Codebase Context

- Provider adapters normalize Codex/Claude `Bash` and Antigravity `run_command` payloads into `PermissionHookRequest.command`.
- The only production safety evaluation occurs in `src/brain/permission_hook.rs` immediately before provider policy/model inference.
- Deterministic denies already bypass inference and remain enforceable across audit-store failures.
- Malformed or unsupported requests already preserve native confirmation: no hook response for Codex/Claude and `ask` for Antigravity.
- Hook payloads are bounded to 64 KiB, while release targets include x86_64/aarch64 Darwin and static musl Linux.
- The root package and both workspace crates use Rust 1.88 and edition 2024. The parser belongs only in the root package; Core and TUI should not acquire it.
- The previously approved reopened-MSOS design explicitly rejected a full parser and constrained Task 1 to `safety.rs`. Parser adoption therefore requires a replacement design and plan rather than silently expanding the current task.

## Recommendations

1. Replace the current reopened-MSOS design with a parser-backed design; do not continue extending the hand-written lexer.
2. Spike pinned `brush-parser = 0.4.0` in tests first. Do not link `brush-core`, execute expansions, or retain third-party AST types outside a private adapter.
3. Freeze every existing and review-discovered case before migration, including `/bin/r[]m]`, adjacent and quoted redirects, `/{,}`, process substitution, ANSI-C strings, variable flags, literal braces, arithmetic compounds, and `[;]`.
4. Require an exhaustive project-owned IR/visitor for commands, word parts, redirects, source spans, and unsupported variants. Treat parse errors, unsupported AST variants, panics, excessive nesting, and resource-limit failures as `Indeterminate`.
5. Change the safety-hook contract so `Indeterminate` preserves native confirmation and never invokes model inference.
6. Keep nested shell strings in `codexctl-dchq`; do not claim the parser migration solves runtime expansion or `sh -c`.
7. Before adoption, verify the full corpus, malformed syntax, Unicode spans, 64 KiB input, deep nesting, provider no-inference behavior, Darwin/musl builds, and release binary/dependency deltas.

## Recommended Beads

No new bead is required yet. Replace the current `codexctl-glet` design/plan after approval, then revise its implementation tasks around the parser spike and explicit indeterminate outcome. Keep `codexctl-dchq` separate for nested shell strings.

## Open Questions

- Does `brush-parser` preserve complete source locations for every word fragment required by the corpus, or must the adapter pair AST nodes with original-source slices?
- Does its public pattern/brace API classify escaped bracket members and initial `]` exactly like Bash for every regression shape?
- What are the compile-time, release-binary-size, and four-target build deltas of adding only `brush-parser`?
- What nesting and input limits prevent stack exhaustion or disproportionate parse time at the 64 KiB hook boundary?

## Refuted / Discarded Claims

- **“The hand lexer needs one more patch.”** Repeated adjacent Bash cases invalidated this after multiple fix/review loops; the same coarse token provenance caused both bypasses and false positives.
- **“Tree-sitter makes the current policy semantic.”** It supplies syntax structure and recovery diagnostics, but Bash's post-parse expansion phases still require word-level policy decisions.
- **“A parser solves nested `sh -c`.”** The command string is an argument in the outer AST and needs an explicit recursive policy owned by `codexctl-dchq`.
- **“Parse failure should deterministically deny.”** The established provider contract preserves native confirmation for malformed/unsupported input; the required property is that failure never reaches inference.

## Sources

- [brush-parser 0.4.0 API](https://docs.rs/brush-parser/0.4.0/brush_parser/) — Primary/Official — 2026-07-29 — parser crate, license, modules, and version. [S1]
- [brush-parser AST](https://docs.rs/brush-parser/0.4.0/brush_parser/ast/index.html) — Primary/Official — 2026-07-29 — groups, redirects, process substitution, and AST types. [S2]
- [brush-parser word parser](https://docs.rs/brush-parser/0.4.0/brush_parser/word/index.html) — Primary/Official — 2026-07-29 — quoting and expansion word pieces. [S3]
- [brush 0.4.0 release notes](https://brush.sh/releases/) — Primary/Official — 2026-07-29 — parser status, compatibility work, and Rust 1.88 MSRV. [S1]
- [tree-sitter-bash 0.25.1 API](https://docs.rs/tree-sitter-bash/0.25.1/tree_sitter_bash/) — Primary/Official — 2026-07-29 — version, language registration, node types, and parse example. [S4]
- [Tree-sitter Node API](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html) — Primary/Official — 2026-07-29 — source spans and error/missing-node inspection. [S5]
- [Tree-sitter syntax and recovery queries](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/1-syntax.html) — Primary/Official — 2026-07-29 — `ERROR` and `MISSING` recovery behavior. [S6]
- [tree-sitter-bash grammar](https://github.com/tree-sitter/tree-sitter-bash/blob/master/grammar.js) — Primary/Official — 2026-07-29 — named Bash syntax constructs and brace-expression rule. [S7]
- [GNU Bash shell expansions](https://www.gnu.org/software/bash/manual/html_node/Shell-Expansions.html) — Primary/Official — 2026-07-29 — expansion ordering after parsing. [S8]
- [`safety::evaluate`](../../src/brain/safety.rs) and [permission-hook flow](../../src/brain/permission_hook.rs) — Primary/Codebase — 2026-07-29 — current `Option` contract and inference fallthrough. [S9]
- [ADR-0004](../../docs/decisions/ADR-0004-provider-aware-guards-and-terminal-actuation.md) — Primary/Codebase — 2026-07-29 — native-confirmation behavior for malformed/unsupported input. [S10]
- [GNU Bash invocation](https://www.gnu.org/software/bash/manual/html_node/Invoking-Bash.html) — Primary/Official — 2026-07-29 — `-c` command-string semantics. [S11]
