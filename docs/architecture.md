# Plugin Architecture

This document describes the design rationale behind the AugurRS plugin system, how it compares to plugin systems in other scientific tools, and the tradeoffs involved.

## Design Goals

1. **Keep `augur-core` domain-free.** The camera SDK knows nothing about microscopy, localization, or any specific analysis domain. All domain logic lives in plugins.
2. **Compile-time safety.** Plugins are statically linked Rust crates. There is no dynamic loading, no reflection, and no runtime classpath scanning. Type errors are caught at compile time.
3. **Familiar to researchers.** The plugin API should feel natural to anyone who has written an ImageJ plugin, a napari extension, or a Micro-Manager device adapter.
4. **Minimal registration overhead.** Adding a plugin is one Cargo dependency line plus one registration line. Removing it is the reverse.

## Execution Model

```
Preview frame arrives
    │
    ├─ Phase 1: FrameOnly plugins (cheap, no event materialization)
    │     Hotpixel Detection, ROI Grid
    │
    ├─ Phase 2: RawEvents plugins (raw CdEvent stream available)
    │     Molecule Localization
    │
    └─ Phase 3: DerivedData plugins (consume upstream results)
          Focus Metrics (reads LocalizationResults)
```

The three-phase model ensures that upstream plugins always run before downstream consumers within the same frame. This is conceptually similar to ImageJ2's service ordering and napari's contribution layering, but enforced at the type level through `PluginInput` declarations.

## Context Bus

The `PluginContext` is a `HashMap<TypeId, Box<dyn Any>>` — a type-indexed store that plugins use to pass results within a single frame. This approach is intentionally simple:

- No runtime string-based lookup (unlike many message bus systems)
- No serialization overhead (data stays as native Rust types)
- No coupling between publisher and consumer (they only share the result type)
- Cleared automatically between frames

The design draws on the SciJava parameter injection model (where `@Parameter` annotations wire services together), but uses Rust's type system instead of runtime annotation processing.

## Tradeoffs

| Decision | Benefit | Cost |
|---|---|---|
| Compile-time linking | Type safety, no runtime discovery failures | Requires recompilation to add/remove plugins |
| Phased execution | Deterministic ordering, no race conditions | Plugins cannot run concurrently within a frame |
| TypeId-indexed context | Zero-overhead typed data sharing | Publisher and consumer must agree on the exact Rust type |
| Separate repository | Core SDK stays clean, plugins are opt-in | Two repositories to manage |

## Future Directions

- **Plugin API crate extraction:** Moving `AnalysisPlugin`, `PluginContext`, and `PluginInput` into a standalone `augur-plugin-api` crate would eliminate the current need for plugins to be compiled as part of `augur-gui`. This is the primary structural improvement planned.
- **Dynamic loading via `libloading`:** For workflows where recompilation is not practical, dynamic shared library loading could be added as an opt-in alternative.
- **Registry index:** A machine-readable index of available plugins could enable tooling for automated dependency resolution and version management.
