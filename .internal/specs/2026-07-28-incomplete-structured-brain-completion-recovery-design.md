# Recover once from incomplete structured Brain completions

> **Date:** 2026-07-28
> **Issue:** codexctl-b7oe
> **Status:** Approved and stress-tested

## Context

Decision inference accepts two provider protocols. Native Ollama responses carry
generated text in `response`; OpenAI-compatible responses carry it in
`choices[0].message.content`. The endpoint-specific extractors already give API
errors precedence over generated content.

A successful provider envelope does not guarantee that its generated string is
valid decision JSON. The reported Ollama response had normal completion
metadata but ended partway through the generated object. The client discarded
that metadata and surfaced serde's `EOF while parsing an object` message, which
the permission hook persisted as a zero-confidence error in Live Needs
Attention.

The fix must preserve the safety boundary: partial JSON can never become an
allow or deny decision.

## Decision

Classify generated decision failures inside `src/brain/client.rs`, then retry
when the generated content is not syntactically valid JSON. A private
`MAX_STRUCTURED_COMPLETION_ATTEMPTS` constant sets the total attempt count to
two.

Each attempt is independent. The client will never concatenate attempts,
complete missing syntax, extract an apparent `action` from partial text, or
otherwise repair model output. Only a fully parsed decision can return from
`infer`.

Do not retry:

- transport or subprocess failures;
- malformed provider envelopes;
- provider API errors;
- valid JSON that lacks `action`, contains an unknown action, or otherwise
  fails the decision schema.

This keeps valid-but-schema-invalid decisions distinct from incomplete or
malformed generated JSON and from provider failures.

## Parsing and error classification

Keep the public string-returning parser behavior used by existing callers and
tests. Add a private classified path for inference with these internal
outcomes:

- provider or transport failure;
- generated JSON incomplete at end of input;
- generated JSON malformed for another syntax reason;
- generated JSON valid but decision schema invalid;
- valid `BrainSuggestion`.

Use serde's error category to distinguish end-of-input from other syntax
errors. Both generated syntax classes are retryable once. Schema validation
runs only after the generated string parses as a complete JSON value.

Ollama and OpenAI-compatible envelope handling remains separate. A recognized
API error in either envelope still wins over any generated content in that
same response.

## Retry bound

The inference call has at most
`MAX_STRUCTURED_COMPLETION_ATTEMPTS` provider attempts and no backoff, polling,
or later background retry. Keep the constant private and fixed at two; adding a
configuration field is deferred until operational evidence requires tuning.

The configured `timeout_ms` remains the total inference budget. Start a
monotonic deadline before the first request. A second request is allowed only
when at least one whole second remains, matching the existing curl timeout
granularity, and receives only the remaining budget. If the first malformed
completion consumes the budget, inference fails immediately with a reason that
the retry was skipped.

The permission hook sees only the final inference result. A successful retry
can therefore produce at most one allow or deny response, while retry
exhaustion produces one fail-safe abstention and one terminal error activity.

If the second attempt fails for a different reason, preserve that attempt's
actual classification. For example, an Ollama API error after malformed output
remains an Ollama API error, with bounded context that the request was retried
after a malformed first completion.

## Diagnostic metadata

When generated syntax remains invalid, return a fixed, actionable reason such
as:

```text
Ollama returned an incomplete structured decision after 2 attempts
(done=true, done_reason=stop, eval_count=746); no action was taken
```

Retain only small completion fields:

- Ollama: `done`, `done_reason`, and `eval_count`;
- OpenAI-compatible: `finish_reason` when present;
- attempt count and whether the retry was skipped for lack of budget.

Boolean and integer fields are copied directly. Provider reason strings are
redacted, character-bounded, and rendered as diagnostic data rather than
trusted prose. Do not retain the prompt, generated response, model name,
timestamps, context tokens, or duration fields.

A successful retry creates no diagnostic event. Final failures continue
through the existing bounded, redacted permission-activity path, so Live
evidence shows the actionable reason without adding a second unresolved
lifecycle. No TUI layout or activity schema change is required.

## Security and failure behavior

- Partial model output never authorizes or denies a tool.
- Retry eligibility comes from JSON parsing, not an apparent field found in
  partial text.
- Provider metadata is untrusted and must be bounded before formatting.
- A retry cannot produce a second hook response or a second decision record.
- API-error precedence remains unchanged on both attempts.
- Any unexpected internal classification failure becomes a fail-safe inference
  error.

## Regression tests

Add focused coverage proving:

1. A valid Ollama envelope with realistic completion metadata and a response
   ending mid-object is classified as incomplete generated content.
2. Inference retries that case once and succeeds when the second independent
   response contains a valid decision.
3. Two incomplete responses make exactly two provider calls, return an
   actionable metadata-only error, and expose none of the prompt or generated
   text.
4. An exhausted timeout budget suppresses the retry.
5. Other malformed generated JSON is classified separately from incomplete
   JSON and receives the same one-retry bound.
6. Valid JSON without `action` is not retried.
7. Ollama and OpenAI API errors are not retried and still take precedence over
   generated content.
8. Existing successful Ollama and OpenAI-compatible decisions remain valid.
9. The permission path persists one error lifecycle with the actionable reason
   and emits no executable response after retry exhaustion.

The first regression must fail before production code changes.

The fake-curl sequencing fixtures must follow the stable-shell design tracked
by the Nix fixture deflake: scripts remain non-executable and are invoked
through the test shell. Do not reproduce the temporary executable pattern.

## Scope

Change `src/brain/client.rs` and the narrow permission-path regression coverage
needed to prove the persisted result. Do not change configuration, public
provider selection, activity schemas, TUI layout, recovery-session delivery,
or user-facing documentation. This retry policy applies to permission-decision
inference only; `infer_recovery` keeps its existing behavior until that path
produces equivalent failure evidence.

## Verification

Run:

1. focused Brain client and permission-hook tests;
2. `cargo fmt --check`;
3. `cargo test`;
4. `cargo clippy --all-targets -- -D warnings`;
5. `cargo build`.

## Stress Test Results: Incomplete structured Brain completion recovery

### Resolved Decisions

- **Retry boundary:** Retry all generated JSON syntax failures. Label
  end-of-input failures as incomplete and other syntax failures as malformed;
  never retry valid JSON that fails the decision schema.
- **Attempt cap:** Use the private
  `MAX_STRUCTURED_COMPLETION_ATTEMPTS` constant with a value of two. Do not add
  configuration without operational evidence.
- **Provider parity:** Apply the same attempt policy to Ollama and
  OpenAI-compatible decision inference while preserving their separate
  envelope schemas and diagnostic fields.
- **Timeout:** Share the existing `timeout_ms` across both attempts. Start the
  second request only when at least one whole second remains and pass curl only
  that remaining budget.
- **Final error:** Preserve the second attempt's real failure class and add
  bounded context about the malformed first attempt.
- **Concurrency:** Keep retries local to an inference call. Do not add a global
  queue, semaphore, backoff, or background work.
- **Activity and delivery:** Intermediate failures remain ephemeral. Only the
  final result reaches persistence, and at most one executable hook response
  can be emitted.
- **Metadata safety:** Allow only `done`, redacted and bounded `done_reason`,
  `eval_count`, and redacted and bounded `finish_reason`. Raw prompts and model
  output are excluded from immediate and persisted diagnostics.
- **Fixture compatibility:** Reconcile with the stable-shell fake-curl change
  before final validation; new tests must not recreate executable temporary
  scripts.
- **Recovery scope:** Leave `infer_recovery` unchanged because its parser and
  delivery path have separate semantics and no matching failure evidence.

### Changes Made

- Replaced the literal one-retry wording with a named private attempt constant.
- Defined second-attempt error precedence and fixed total timeout accounting.
- Added the Nix fake-curl compatibility constraint.
- Made permission-decision-only scope explicit.

### Deferred / Parking Lot

- Promote the attempt cap to bounded configuration only if runtime evidence
  shows that operators need to tune it.
- Extend syntax recovery to `infer_recovery` only after an equivalent failure
  is observed and its separate delivery semantics are designed.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** The in-progress fake-curl fixture change overlaps the
  client test helper, so final validation must use its stable-shell form.
