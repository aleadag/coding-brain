# Surface LLM API errors accurately

> **Date:** 2026-07-27
> **Issue:** codexctl-pgto
> **Status:** Approved design

## Context

The brain client invokes the configured LLM endpoint through `curl` and
receives the response body even when the server returns an HTTP error. A native
Ollama response such as:

```json
{"error":"unable to load model: ..."}
```

is valid JSON, but `parse_ollama_response` currently treats any envelope
without a string `response` field as model-generated decision JSON.
`parse_suggestion_json` then reports `missing 'action' field`, hiding the API
error that caused inference to fail.

The OpenAI-compatible parser has the same fallback for envelopes without
`choices[0].message.content`. The completion path used by recovery inference
also returns either malformed envelope as if it were generated content.

The configured endpoint path already selects the protocol:

- `/api/generate` uses the native Ollama request and response format.
- `/v1/chat/completions` uses the OpenAI-compatible request and response
  format.

## Decision

Keep endpoint selection unchanged. Introduce two private, schema-specific
content extractors in `src/brain/client.rs`.

The native Ollama extractor will:

1. Return `Ollama API error: <message>` for the documented string `error`
   field.
2. Otherwise return the string `response` field.
3. Return `invalid Ollama response` when neither documented shape matches.

The OpenAI-compatible extractor will:

1. Return `OpenAI API error: <message>` for a structured OpenAI error envelope
   containing `error.message`.
2. Otherwise return the string `choices[0].message.content` field.
3. Return `invalid OpenAI response` when neither documented shape
   matches.

Share only the small error-formatting mechanic; do not apply one provider's
envelope assumptions before protocol selection.

Use each extractor in both its decision-response parser and its corresponding
branch of `call_llm`. Only successfully extracted generated content reaches
`parse_suggestion_json` or the recovery parser.

This keeps provider-envelope classification separate from generated-decision
validation. A valid provider envelope containing malformed decision JSON
remains a suggestion-format error such as `missing 'action' field`; an API
error envelope reports the provider message; and an unrecognized envelope
reports an invalid provider response.

## Error Handling and Security

Each extractor accepts only its explicitly supported success and error shapes.
Unexpected shapes are provider-response errors rather than guessed API errors
or model-generated decisions.

The existing response-size bound remains in force before parsing. The change
does not add redirects, retries, logging, network access, or new persistence.
It reports the provider-supplied message through the existing inference error
path. Immediate command output retains the provider message for diagnosis;
persisted hook activity continues to use the existing bounded secret
redaction.

HTTP status capture remains out of scope. The observed Ollama contract includes
an explicit error envelope, so changing the curl subprocess protocol would add
unnecessary transport complexity for this fix. Non-JSON proxy or server errors
continue to report invalid JSON rather than an HTTP status.

## Regression Tests

Add focused unit coverage in `src/brain/client.rs` proving:

1. A native Ollama string error reports `Ollama API error` and the provider
   message.
2. A normal native Ollama envelope extracts and parses its string `response`.
3. A malformed native Ollama envelope reports `invalid Ollama response`.
4. An OpenAI structured error reports `OpenAI API error` and its message.
5. A normal OpenAI envelope extracts and parses
   `choices[0].message.content`.
6. A malformed OpenAI envelope reports `invalid OpenAI response`.
7. Successfully extracted content that lacks `action` still reports the
   suggestion-format error rather than a provider-response error.

The first test must fail against the current implementation before the
production change is made.

## Scope

Change only `src/brain/client.rs`. Do not alter configuration, public APIs,
decision semantics, recovery semantics, activity persistence, curl arguments,
provider selection, or user-facing documentation.

## Verification

Run:

1. The focused brain-client regression tests.
2. `cargo fmt --check`.
3. `cargo test`.
4. `cargo clippy -- -D warnings`.

## Stress Test Results: LLM API error handling

### Resolved Decisions

- **Classification boundary:** Endpoint-selected extractors validate provider
  envelopes before generated content reaches decision or recovery parsing.
- **Envelope precedence:** A recognized error wins over generated content so a
  partial response from a failed request cannot become a permission decision.
- **Schema ownership:** Ollama and OpenAI-compatible extractors own distinct
  documented shapes; there is no generic pre-parser `error` assumption.
- **Malformed envelopes:** Wrong-shaped provider data reports an invalid
  provider response, never a malformed decision.
- **Transport status:** HTTP status capture remains out of scope because the
  evidenced failure has a parseable API envelope and changing curl plumbing
  would affect every provider.
- **Disclosure:** Immediate diagnostics retain the provider message, while
  persisted activity keeps its existing bounded redaction.
- **Scale:** Extraction borrows from one already-parsed, size-bounded JSON
  value; no successful response body clone or second parse is introduced.
- **Testing and rollback:** Focused extractor and parser tests cover both
  formats. The change remains a one-file revert with no state migration.

### Changes Made

- Expanded the design from native Ollama errors to both supported endpoint
  formats.
- Replaced the generic Ollama extractor with two schema-specific extractors.
- Removed fallback of malformed provider envelopes into generated-decision
  parsing.
- Added explicit OpenAI-compatible error, success, and malformed-envelope
  coverage.

### Deferred / Parking Lot

- Capturing HTTP response status and proxy-generated non-JSON errors remains
  deferred until evidence requires transport-level classification.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Other OpenAI-compatible servers may extend the standard
  response schema; unsupported shapes intentionally remain invalid provider
  responses until grounded by evidence.
