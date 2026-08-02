# BusyBox `time -f` Value Semantics

## Context

The deterministic shell-safety evaluator classifies wrapper option values before projecting a wrapped child command. BusyBox 1.36.1 accepts an empty `time -f` format and still executes the child, but the evaluator currently applies the `NonEmpty` rule shared with `time -o`. It therefore returns `Indeterminate` instead of inspecting and denying a destructive child.

Real-binary differential probes establish that `time -o` is different: empty and directory-valued output paths fail before the child runs. This change must not broaden `-o` classification.

## Design

Assign BusyBox `time -f` the existing `WrapperValueRule::Any` semantics while retaining `WrapperValueRule::NonEmpty` for BusyBox `time -o`.

This preserves the existing wrapper scanner and typed value-consumption path:

- A literal empty or nonempty `-f` value is consumed and the child is analyzed.
- A separate dynamic value proven to expand to exactly one argument is consumed because every resulting format value is valid for command-position analysis.
- An attached dynamic value still requires proof that the attachment cannot disappear; otherwise BusyBox may consume the following command word as the format.
- Values that may expand to zero or multiple arguments retain the existing unsafe-expansion handling.
- Unknown options, missing values, terminating options, and unsupported Toybox forms remain `Indeterminate` before model inference.
- Deterministic destructive children remain denied before Codex, Claude, or Antigravity model inference.

No new abstraction or configuration is required.

## Testing

Add regressions at the three existing acceptance layers:

1. Evaluator tests prove literal empty and exact-one dynamic BusyBox `time -f` values reach destructive-child classification, while multi-argument expansion remains fail-closed.
2. Shipped-helper tests prove the same commands return `deny` with `irreversible-root-delete`.
3. Provider tests prove these deterministic denials issue zero model requests for Codex, Claude, and Antigravity.

Replay harmless BusyBox probes to confirm empty and dynamic-empty `-f` values execute their child. Run the focused tests, then the serial all-target suite, Clippy with warnings denied, rustfmt check, and Cargo build through the repository's Nix development shell.

## Scope

This change addresses only BusyBox `time -f` format semantics. The broader question of proving whether arbitrary nonempty `time -o` paths permit child execution predates this regression and is not changed here.
