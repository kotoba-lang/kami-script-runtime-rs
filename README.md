# kami-script-runtime-rs

The missing WASM **host** half of the kotoba-lang engine stack. `kotoba-lang/engine`
(CLJC) compiles a `.clj` game script to a Module IR and, since
[kotoba-lang/engine#1](https://github.com/kotoba-lang/engine/pull/1), to real
`.wasm` bytes. This crate loads those bytes with [`wasmtime`](https://wasmtime.dev/),
binds the `kami:engine/*` host-imports the compiler's codegen actually emits, and
drives the guest's `init`/`<name>-tick` lifecycle against a minimal in-memory ECS
store.

## Why this repo exists

The original `kami-script-runtime` (inside `kotoba-lang/kami-engine`) was deleted
along with that repo's entire Rust workspace during the clj-wgsl migration
(`kotoba-lang/kotoba` PR #259, commit `604896171b`), even though that migration's
own ledger (`90-docs/adr/2607010930-clj-wgsl-migration.edn` in the
`com-junkawasaki/root` superproject) classified `kotoba-runtime + kami-script-runtime
(WASM host)` as `:stay-Rust-substrate` — it was never supposed to be removed without
a replacement. The CLJC-scoped `kotoba-lang/kami-script-runtime` repo that exists
today explicitly, permanently excludes this exact functionality from its own scope
(see its README: only `input_map.rs` was portable, `lib.rs`/`platform.rs`/`bin/*`
"won't be ported"). This repo restores that missing piece as its own standalone
Rust crate — the WASM host **cannot** live in a CLJC-only repo, and
`kotoba-lang/kami-engine` runs a CI "no-rust guard," so neither existing repo could
host it.

## Provenance

The original source was recovered read-only from `kami-engine`'s git history at
`a8368f9c0d784dbc9d11e8fa8f407aa95c7ce4fa:kami-script-runtime/src/lib.rs`. Every
host-import binding in `src/lib.rs` (`bind_scene`/`bind_input`/`bind_random`/
`bind_time`) is ported 1:1 from that recovered source, scoped to the **14
host-imports** `kotoba.engine-clj.codegen/compile` actually produces for
isekai-network's real `games/01-netsurvivors/logic.clj` — verified by compiling it
and inspecting `:host-imports` before writing a single binding, not assumed from
the WIT contract alone.

## What's ported vs. simplified vs. not done

| | |
|---|---|
| **Ported 1:1** | `bind_scene` (spawn/despawn/get-x/get-y/set-position/set-velocity/count-tagged/query-begin/query-next/nearest/move-toward), `bind_input::axis`, `bind_random::int` (xorshift64, host-seeded), `bind_time::tick`, `ordered_tick_exports` (hand-parses the WASM export section so system execution order matches guest definition order — engine `exports()` iteration order isn't guaranteed stable), fixed-step Euler `integrate` |
| **Simplified** | Entity storage is a hand-rolled `HashMap`-based store (`EcsStore`), not `hecs` — semantics ported verbatim (tag index, position/velocity fields, query cursors, id allocation), only the storage backend differs. No `wasmi` no-JIT backend (only `wasmtime`/JIT) |
| **Not done** | `render`/`audio`/`physics` bindings (the original had these as queued no-op/stub bindings for a Phase-3 wgpu/rapier/kami-audio integration that was never built even before deletion — nothing regressed by leaving them out here) |

## Verification — real, not just "it compiled"

`src/bin/kami_host.rs` loads a `.wasm` file, calls `init`, runs N ticks, and reports
entity counts (`DEBUG_POS=1` env var also dumps per-entity position/velocity).
Running it against isekai-network's actual compiled `games/01-netsurvivors/logic.clj`
(`tests/fixtures/isekai-network-01-netsurvivors.wasm`) for 300 ticks:

```
tick    0: entities=   1 shiro-pico=1 ghost=0 beat-spark=0
tick  100: entities=   8 shiro-pico=1 ghost=7 beat-spark=0
tick  200: entities=  14 shiro-pico=1 ghost=13 beat-spark=0
tick  240: entities=  15 shiro-pico=1 ghost=13 beat-spark=1
tick  299: entities=  16 shiro-pico=1 ghost=14 beat-spark=1
```

This is the first time in the isekai-network project's history that its actual
compiled game logic has run and evolved real, observable state: the duo spawns,
ghosts spawn on schedule (`spawn-period`), the AI system moves them toward the duo,
`weapon-tick`'s beat-gated bonus (added to `logic.clj` in a prior iteration) fired
at least once (`beat-spark=1`), and — via `cargo test` — the deterministic
`weapon_culls_enemies_in_range`-style scenario (3 enemies, weapon system purifies
them one per tick within range) is covered directly.

**A real bug was found and fixed during this verification**, not in this crate but
in `kotoba-lang/engine`: inline (not `def`-bound) f32 literals were encoded from
their raw Clojure float value instead of their IEEE-754 bit pattern, so e.g. every
`(f32 -520.0)` used directly in a `cond` branch decoded back to `NaN` — ghosts spawned
at those positions never moved. Fixed upstream in
[kotoba-lang/engine#2](https://github.com/kotoba-lang/engine/pull/2) (not merged by
this repo — needs its own review). `tests/fixtures/isekai-network-01-netsurvivors.wasm`
in this repo was regenerated with that fix applied.

## Build / run

```bash
cargo build --release
./target/release/kami-host tests/fixtures/isekai-network-01-netsurvivors.wasm 300 7
DEBUG_POS=1 ./target/release/kami-host tests/fixtures/isekai-network-01-netsurvivors.wasm 100 7
cargo test
```

## Not yet done (honest scope)

- `wasmi` no-JIT backend (iOS/PS5/Switch) — only `wasmtime` (JIT) is implemented.
- The host-import type-index verification gap [kotoba-lang/engine#1](https://github.com/kotoba-lang/engine/pull/1)'s
  review flagged (does a given import's WASM-level typeidx actually match its
  semantically-intended signature, not just an in-bounds index) is now
  *practically* exercised by this repo's real instantiation+execution — no
  `wasmtime` instantiation error occurred across all 14 host-imports, which is
  meaningful evidence the indices are correct, though not a dedicated unit test
  isolating that claim.
- `render`/`audio`/`physics` bindings (see table above).
- Golden-frame determinism testing across `wasmtime` vs. a future `wasmi` backend
  (the original's ADR-0037 claim — untestable until `wasmi` exists here).

## wasmi (no-JIT) backend

`KamiHostWasmi` (`src/wasmi_host.rs`) is a `wasmi`-backed twin of `KamiHost`
with the identical public API and the same 14 host-import bindings, ported
in parallel rather than behind a shared trait (kept simple for a first
pass — a shared trait/generic-over-backend refactor is a reasonable
follow-up, not done here). `tests/wasmi_parity.rs` runs isekai-network's
real compiled module through both backends for 200 ticks and asserts
bit-for-bit identical entity state (position/velocity/tag/count) — this is
the "golden-frame" parity the original recovered design's docs called for
("two WASM backends, one binding codebase... deterministic: both backends
produce bit-identical runs"), verified for real, not assumed.
