# 049 — Universal campaign catalog, deadlines and terminal identity

Date: 2026-09-10. Status: accepted for source implementation; bench acceptance pending.

## Context

Universal display points did not define delegated recorder rows. Old aliases
loaded different, longer studies. A background-only screen could not select
useful response, and A4 lost ON/OFF overrides. Active Continue and A5 button
dispatch also lacked a reliable terminal handoff.

## Decision

Generate actual owner files and a compiled catalog together. Validate manifest
estimates against the catalog. Keep acquisition and device leases in existing
owners; the universal runner sequences readiness, optical-reference release,
selected camera state and terminal completion.

The execute request gains optional absolute acquisition deadline and an attempt
number (legacy default zero). Owners expose readiness and safe-stop services and
publish explicit terminal states with measurement and attempt identity. A4 adds
the constant-reference service. The runner matches replies to request identity.
All participating plugins must be installed together.

Freeze plan and selected state; retain journal and checkpoints. Resume uses the
original campaign start time. Optional coverage is dropped in a declared order;
failed or omitted coverage is not counted as recorded. Storage errors stop the
campaign, and stop waits for owner finalization rather than abandoning files.

## Consequences

Both full main levels fit a five-hour nominal operator plan plus one-hour reserve.
Estimates are reviewable and cannot diverge silently from compiled acquisition
rows. Safe cleanup can exceed a deadline after hardware/storage failure; the
runner must report this instead of claiming a physically guaranteed finish.
The new bundle needs Windows saved-data validation. No new controller firmware
or host service API is introduced.
