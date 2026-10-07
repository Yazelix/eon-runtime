# Agent Guidelines

Read README.md and the affected source before changing this repository.

## Ownership

This repository owns the cohesive `eon-runtime` library and canonical
`eon-workspace-protocol` (EONW). Runtime modules stay private; consumers invoke
`run(Inputs)` through the concrete root exports. Eon supplies product version,
defaults, validated component facts, launch paths and immutable assembly inputs.
Runtime owns configuration parsing, lifecycle, workspace state and generation.
Orbit owns terminals and terminal state; Venus owns native presentation.

Keep runtime and codec package versions and source identities independent.
Preserve historical EONW revisions; equal version labels do not prove package
equivalence. Consumers must not require codec and runtime repository revisions
to advance together when complete codec package/build inputs are unchanged.

Do not add product defaults, a graph parser, Eon source includes, a binary host,
daemon, framework, platform promise or release channel without user direction.
Rust owns implementation; Eon owns Nix composition and product distribution.

## Workflow

Work on `edge`. `main` and `stable` are promotion-only channels, preserving
`stable ⊆ main ⊆ edge`; do not create or promote them without user direction.
Preserve concurrent work and live terminals. Never reset, force-push or restart
user processes automatically.

The extraction plan and acceptance records live in Eon's Beads graph. Use `br`
there for the selected issue; do not create a duplicate tracker here. Eon's
local runtime/codec trees at `e431d75` are frozen migration inputs until cutover.
Subsequent runtime changes belong here.

Use README's verification commands. Prove changed behavior at its owner and
consumer boundary; retain exact revisions, environment, results and limits.
Update README's LOC scorecard when handwritten project files change. Count
tracked text and code, excluding Git data, lock files and generated artifacts.
