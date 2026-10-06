# Eon Runtime

The runtime library and canonical workspace protocol used by Eon and Venus.
The repository builds independently of Eon's product checkout.

| Package | Owns | Version |
|---|---|---|
| `eon-runtime` | Invocation, Session lifecycle, workspace state, configuration parsing and generation identity | `0.1.0` |
| `eon-workspace-protocol` | EONW v2–v7 types and bounded codecs | `0.1.0` |

## Consumer boundary

Eon's actual executable calls `eon_runtime::run(inputs)`. The concrete root
exports construct `Inputs`, `ComponentFacts`, defaults and managed-program
inputs; the nine mechanism modules remain private. `run` returns the product
label and exit result. Eon retains process exit/error handling.

Eon supplies its own product version, chosen defaults, validated component
report and Orbit revision, opaque executable paths, and immutable assembly
contribution. The runtime has no dependency on Eon, its component graph,
validator, product defaults, assets or Nix expressions.

Runtime combines its own immutable runtime/EONW bytes with the supplied assembly
contribution to compute one `g1-` generation. Mutable configuration, live state
and store/profile paths do not become generation identity. Orbit remains the
Session/terminal owner; Venus remains the native presentation owner.

The accepted concrete assembly is illustrated by Eon's
[product inputs](https://github.com/Yazelix/eon/blob/e431d7583c84ff574054d2edf556e9827114987e/crates/eon/src/product.rs).
Select Git dependencies by exact accepted commit and package name; crates are
not published to a registry. Eon owns product composition and distribution.

## Package provenance

The transfer source is Eon
[`e431d75`](https://github.com/Yazelix/eon/commit/e431d7583c84ff574054d2edf556e9827114987e).
Both complete crate trees, including manifests and unit tests, are unchanged:

| Package | Original Git subtree |
|---|---|
| `eon-runtime` | `3dcfef6bf31758233f2a33eb31d5184ebeae9843` |
| `eon-workspace-protocol` | `8409c419a3496d6f01eeef65295f5d95b943313e` |

The codec tree also equals Venus's historical Eon dependency at
`f41a41c9aecc4c436edfa2f394b832aa6d8711ad`. Runtime's exact Orbit protocol
dependency remains `b6cecf8f2ee35570b41cfdc578b095889d917fe2`.

Runtime and codec have separate package manifests and version owners. They may
initially share a repository commit without requiring future revisions to
match. A runtime-only change can retain a prior accepted codec pin when its
complete package and relevant build inputs remain identical. Matching SemVer
or wire labels alone is insufficient.

EONW's complete package is its Cargo manifest and `src/lib.rs`, `src/v2.rs`
through `src/v7.rs`. It has no external dependencies, feature declarations,
build script or workspace-inherited settings. The root workspace uses resolver
3 and declares no shared metadata, dependencies, patches or build profiles.
Changes to those facts require renewed package/build-input evidence.

Eon's `eon-runtime-producer-68hq` holds the exact accepted producer revision and
verification evidence. Consumer rebinding and installed Eon cutover are separate
stages. Eon's local copies at `e431d75` remain frozen until that cutover; runtime
evolution has one owner here.

## Development

Use Rust 1.95 or later. From this checkout:

```sh
cargo fmt --all --check
cargo check --locked --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Tests use independent supplied inputs. Eon's actual executable integration
tests stay with Eon and are exercised against exact producer candidates during
integration. Wire/source comparison does not claim a new native platform proof.
The composed product is proved on x86_64 Linux native Wayland; Apple Silicon
macOS remains unproved for Eon. This transfer expands no platform support.

## License

[Apache-2.0](LICENSE). [NOTICE](NOTICE) records the immutable transfer source.
Eon assets and their unrelated third-party notices are not part of this source
library.

## LOC scorecard

Tracked handwritten text and code; excludes Git data, lock files and generated
artifacts.

| Surface | Lines |
|---|---:|
| Rust source and tests | 13,367 |
| Cargo manifests | 26 |
| Agent guidelines | 38 |
| README | 101 |
| License and notice | 209 |
| Repository ignore rules | 1 |
| **Total** | **13,742** |
