# Delivered Denial Live Projection Design

## Problem

Coding Brain 0.58.0 records an automatic permission denial as a `Denied` decision followed by `Delivered` evidence. Live currently treats that pair as unresolved: it places the activity under Needs Attention and renders `denied · response delivered · execution not confirmed`.

The captured case was a Bash permission request for a `gh api` pipeline in Codex session `019f87b3-b4eb-7bf1-bed5-dc4ed0052fc2`. The tool call requested escalated sandbox permissions, and Coding Brain returned a deny response before Codex displayed a native prompt. Successful delivery therefore confirms that Coding Brain blocked the command; a later execution outcome is neither expected nor needed to resolve the Live activity.

## Goals

- Place a delivered model or deterministic denial under Recent rather than Needs Attention.
- Describe the result as blocked and make clear that the command did not execute.
- Keep failed or unknown denial delivery actionable.
- Preserve advisory fallthrough as actionable when Codex may still be waiting for native approval.

## Non-Goals

- Do not change when the permission hook allows, denies, or abstains.
- Do not rename the persisted `Denied` state or migrate existing activity rows.
- Do not hide automatic decisions from Live or Brain Review.
- Do not infer command execution from response delivery for allowed decisions.

## Design

### Activity projection

`project_snapshot` will resolve a denial when its delivery state is `Delivered`, provided no failed outcome or other existing attention condition overrides it. The item remains an `ActivityState::Denied` audit record, but it moves to the Recent collection and no longer contributes to `unresolved_count`.

A denied item with `DeliveryState::Unknown` or `DeliveryState::Failed` remains under Needs Attention. An advisory deny is recorded as abstention without executable delivery, so the existing actionable behavior remains unchanged.

Allowed decisions keep their current semantics. Delivery confirms only that Codex received the allow response; it does not prove that the command later ran.

### Live status text

For `ActivityState::Denied` with `DeliveryState::Delivered` and no later outcome or correction, Live will render:

```text
blocked · command did not execute
```

This special case precedes the generic delivery text. Other states continue to distinguish response delivery from execution outcome, including delivered allows and failed or unknown delivery.

### Error behavior

Delivery failure remains higher-risk evidence and stays in Needs Attention with `delivery failed · execution not confirmed`. Missing delivery evidence stays actionable as `delivery unknown · execution not confirmed`. Existing failed outcomes also remain under Needs Attention.

## Testing

- Add an activity projection regression that appends `Denied` and then `Delivered`, and asserts that the item appears once under Recent with `unresolved_count == 0`.
- Cover denied decisions with each delivery state: `Delivered` appears under Recent, while `Failed` and `Unknown` remain actionable.
- Strengthen the automatic-deny process regression to assert the projected Recent result after successful response delivery.
- Keep the advisory process regression that asserts no executable response and an actionable abstention.
- Update the TUI regression to supply the delivered denial through Recent and assert the exact `blocked · command did not execute` copy.
- Keep the delivered-allow regression unchanged so the new status special case cannot erase the distinction between response delivery and execution evidence.
- Run focused activity, permission-hook, and TUI tests before the full workspace format, test, Clippy, and build gates.

## Acceptance Criteria

- A delivered automatic denial appears under Recent and does not increase the Needs Attention count.
- Live says the command was blocked and did not execute; it does not say execution is unconfirmed.
- Failed or unknown denial delivery remains in Needs Attention.
- Advisory fallthrough remains actionable and emits no executable denial response.
- Persisted decision state and permission-hook behavior do not change.

## Stress Test Results: Delivered Denial Live Projection

### Resolved Decisions

- A valid, flushed deny response is a completed block for Live; allowed responses still require separate execution evidence.
- Delivery failure or a failed outcome overrides earlier successful delivery and remains actionable.
- Advisory deny or abstention remains actionable because it emits no executable response.
- Existing correction and outcome precedence remains unchanged; blocked copy applies only without either.
- Persisted `Denied` and `Delivered` rows remain unchanged, so historical activity is reprojected without migration.
- The implementation stays within the existing projection predicate and renderer instead of adding a new type or abstraction.
- The change affects read-only presentation and does not weaken permission enforcement; unknown and failed delivery remain prominent.
- Regression coverage includes delivered, failed, and unknown denial delivery plus automatic and advisory process paths.

### Changes Made

- Added explicit denied-plus-unknown coverage.
- Added a process-level projection assertion for successful automatic denial.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: None beyond the existing distinction between response delivery and later execution evidence, which remains unchanged for allowed decisions.
