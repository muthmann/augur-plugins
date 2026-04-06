# Lessons

- No project-specific lessons recorded yet.
- When a user instruction conflicts with my higher-priority operating rules, describe the conflict plainly and specifically. Do not say "the environment requires it" when the real cause is my own branch-naming policy.

## Lesson: Treat repo-wide doc updates as secondary when the user asks for runtime compatibility

- Date: 2026-04-06
- Trigger: The user asked to update this repo for a new plugin-system interface, and I focused on rewriting documentation before first proving the plugins themselves were compatible again.
- Rule: For plugin/API migration requests, first compile the affected crates against the current host/API checkout, fix code compatibility issues, verify builds/tests, and only then refresh docs to match the actual implementation state.
- Evidence: Plugin-system migration task in `augur-plugins`.

## Lesson: Re-check branch state after the user updates remotes

- Date: 2026-03-18
- Trigger: The user noted that they had just run `git fetch`, which could have changed the correct base branch for cross-repo implementation work.
- Rule: When the user says fetch/merge state changed, re-run the relevant branch-graph checks before continuing, and keep the current base branch only after confirming the actual commit graph.
- Evidence: `augur-plugins` branch check during the v0.2 plugin API cleanup task.
