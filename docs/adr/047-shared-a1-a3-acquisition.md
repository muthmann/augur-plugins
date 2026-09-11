# ADR 047: Share A1 and A3 acquisition

Date: 2026-09-08
Status: Accepted

## Problem

A3 needs A1's sine protocol acquisition, folder/identity handling, confirmed
commands, record preservation, retries and camera restoration. Copying its state
machine would create a second implementation to maintain. Depending directly on
the A1 runtime crate would link two exported plugin vtables (ADR 031).

## Decision

Move the existing A1 coordinator and pure analysis modules to the non-runtime
`stage-a-sine-acquisition` crate. Keep A1's public module/type re-exports and its
runtime entry point. Add a separate A3 entry point with its own manifest.
A compile-time experiment selector keeps routing IDs, schemas, archived source
names and metadata distinct. A1 uses the original defaults and UI. A3 exposes
only file, folder, identity, Run/resume and Stop, without live analysis or a
cutoff prerequisite. It uses the same firmware A1/J24 sine acquisition mode.

A3 refuses points shorter than five cycles. It preserves early stops as partial
acquisitions that resume cannot reuse. Scientific qualification remains offline,
including actual contrast, frequency independence, drift and per-pixel validity.

## Consequences

There is one shared lifecycle implementation and one vtable per dynamic library.
Source locations change; A1's Rust exports and on-disk scientific schema remain
compatible. Both plugin entry points and the full workspace are regression-tested.
All bundles discover A3 from its manifest using the existing build/install scripts.
No host API or firmware change is required beyond the already paired recording-root
host API. Windows runtime delivery and a physical smoke remain separate checks.
