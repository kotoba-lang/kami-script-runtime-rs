//! kami-script-runtime-rs — the WASM host half of the kotoba-lang engine
//! stack. `kotoba-lang/engine` (CLJC) compiles a `.clj` game script to a
//! Module IR and, since PR kotoba-lang/engine#1, to real `.wasm` bytes. This
//! crate loads those bytes with `wasmtime`, binds every `kami:engine/*`
//! import the compiler's codegen actually emits (verified against a real
//! module compiled from `gftdcojp/isekai-network`'s
//! `games/01-netsurvivors/logic.clj`, not invented), and drives the guest's
//! `init`/`<name>-tick` lifecycle against a minimal in-memory ECS store.
//!
//! ## Provenance
//!
//! The original `kami-script-runtime` (inside `kotoba-lang/kami-engine`,
//! deleted along with that repo's whole Rust workspace during the clj-wgsl
//! migration, commit `604896171b` / PR kotoba-lang/kotoba#259) was recovered
//! read-only from git history at
//! `kami-engine@a8368f9c0d784dbc9d11e8fa8f407aa95c7ce4fa:kami-script-runtime/src/lib.rs`.
//! The migration's own ledger (`90-docs/adr/2607010930-clj-wgsl-migration.edn`
//! in the `com-junkawasaki/root` superproject) classified `kami-script-runtime`
//! as `:stay-Rust-substrate` — it was never supposed to be deleted without a
//! replacement, which is what this repo restores. The host-import bindings
//! below (`bind_scene`/`bind_input`/`bind_random`/`bind_time`) are ported
//! 1:1 from that recovered source for the 14 imports the current compiler
//! actually produces. **Not ported** (honestly out of scope for this pass):
//! `render`/`audio`/`physics` bindings (the original had them as queued
//! no-op/stub bindings for a Phase-3 wgpu/rapier/kami-audio integration that
//! was never built even before deletion — nothing regressed by leaving them
//! out here); the `wasmi` no-JIT backend (only `wasmtime` is implemented);
//! `hecs` as the entity store (a hand-rolled `HashMap`-based store is used
//! instead — see [`Store`] below — to avoid pulling in a pinned external ECS
//! crate for a first pass; semantics are ported 1:1, only the storage
//! backend differs).

use std::collections::HashMap;
use wasmtime::{Caller, Engine, Extern, Instance, Linker, Module, Store as WasmtimeStore};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("wasm backend error: {0}")]
    Backend(#[from] wasmtime::Error),
    #[error("missing export `{0}`")]
    MissingExport(String),
}

// ---------------------------------------------------------------------------
// Entity store — a minimal, hand-rolled substitute for the original's hecs
// world. Ported 1:1 in *semantics* (tag index, position/velocity fields,
// query cursors, id allocation), not in storage mechanism.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct Entity {
    pub pos: [f32; 3],
    pub vel: [f32; 3],
}

#[derive(Default)]
pub struct EcsStore {
    next_id: u32,
    entities: HashMap<u32, Entity>,
    tags: HashMap<u32, String>,
}

impl EcsStore {
    fn spawn(&mut self, tag: &str) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.entities.insert(id, Entity::default());
        if !tag.is_empty() {
            self.tags.insert(id, tag.to_string());
        }
        id
    }

    fn despawn(&mut self, id: u32) {
        self.entities.remove(&id);
        self.tags.remove(&id);
    }

    fn tagged<'a>(&'a self, tag: &str) -> impl Iterator<Item = (u32, &'a Entity)> + 'a {
        let tag = tag.to_string();
        self.entities.iter().filter_map(move |(id, e)| {
            if self.tags.get(id).map(|t| t.as_str()) == Some(tag.as_str()) {
                Some((*id, e))
            } else {
                None
            }
        })
    }
}

/// Shared mutable host state every bound import closure reads/writes.
/// Field-for-field port of the recovered `HostState`, minus the
/// render/audio/physics queues (out of scope, see module docs).
pub struct HostState {
    pub store: EcsStore,
    pub axes: HashMap<String, f32>,
    pub tick_n: i64,
    pub query_cursors: HashMap<i64, Vec<u32>>,
    pub next_query: i64,
    /// xorshift64 state — host-owned seeded PRNG, ported verbatim from the
    /// recovered `bind_random::int` so replays/tests are reproducible.
    pub rng: u64,
}

impl HostState {
    pub fn new(seed: u64) -> Self {
        Self {
            store: EcsStore::default(),
            axes: HashMap::new(),
            tick_n: 0,
            query_cursors: HashMap::new(),
            next_query: 1,
            rng: seed | 1, // forced odd/non-zero so xorshift64 never degenerates
        }
    }

    pub fn set_axis(&mut self, name: &str, value: f32) {
        self.axes.insert(name.to_string(), value);
    }
}

// ---------------------------------------------------------------------------
// Guest-memory string reads (all `:string-handle` params lower to a WASM
// (i32 ptr, i32 len) pair per `kotoba.engine-clj.wasm-bytes/host-import-functype`)
// ---------------------------------------------------------------------------

fn read_guest_str(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> String {
    if len <= 0 {
        return String::new();
    }
    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
        return String::new();
    };
    let data = mem.data(&caller);
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end <= data.len() {
        String::from_utf8_lossy(&data[start..end]).into_owned()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Host-import bindings — ported 1:1 from the recovered lib.rs, scoped to the
// 14 host-imports `kotoba.engine-clj.codegen/compile` actually emitted for
// isekai-network's real `games/01-netsurvivors/logic.clj` (verified by
// compiling it and inspecting `:host-imports` before writing this file, not
// assumed from the WIT contract alone).
// ---------------------------------------------------------------------------

fn bind_scene(linker: &mut Linker<HostState>) -> Result<(), RuntimeError> {
    let m = "kami:engine/scene@1.0.0";

    linker.func_wrap(
        m,
        "spawn",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            let tag = read_guest_str(&mut caller, ptr, len);
            caller.data_mut().store.spawn(&tag) as i64
        },
    )?;

    linker.func_wrap(m, "despawn", |mut caller: Caller<'_, HostState>, eid: i64| {
        caller.data_mut().store.despawn(eid as u32);
    })?;

    linker.func_wrap(
        m,
        "set-position",
        |mut caller: Caller<'_, HostState>, eid: i64, x: f32, y: f32, z: f32| {
            if let Some(e) = caller.data_mut().store.entities.get_mut(&(eid as u32)) {
                e.pos = [x, y, z];
            }
        },
    )?;

    linker.func_wrap(
        m,
        "set-velocity",
        |mut caller: Caller<'_, HostState>, eid: i64, vx: f32, vy: f32, vz: f32| {
            if let Some(e) = caller.data_mut().store.entities.get_mut(&(eid as u32)) {
                e.vel = [vx, vy, vz];
            }
        },
    )?;

    linker.func_wrap(m, "get-x", |caller: Caller<'_, HostState>, eid: i64| -> f32 {
        caller
            .data()
            .store
            .entities
            .get(&(eid as u32))
            .map(|e| e.pos[0])
            .unwrap_or(0.0)
    })?;

    linker.func_wrap(m, "get-y", |caller: Caller<'_, HostState>, eid: i64| -> f32 {
        caller
            .data()
            .store
            .entities
            .get(&(eid as u32))
            .map(|e| e.pos[1])
            .unwrap_or(0.0)
    })?;

    linker.func_wrap(
        m,
        "count-tagged",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            let tag = read_guest_str(&mut caller, ptr, len);
            caller.data().store.tagged(&tag).count() as i64
        },
    )?;

    linker.func_wrap(
        m,
        "query-begin",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            let tag = read_guest_str(&mut caller, ptr, len);
            let ids: Vec<u32> = caller.data().store.tagged(&tag).map(|(id, _)| id).collect();
            let s = caller.data_mut();
            let handle = s.next_query;
            s.next_query += 1;
            s.query_cursors.insert(handle, ids);
            handle
        },
    )?;

    linker.func_wrap(
        m,
        "query-next",
        |mut caller: Caller<'_, HostState>, handle: i64| -> i64 {
            let s = caller.data_mut();
            match s.query_cursors.get_mut(&handle) {
                Some(v) => match v.pop() {
                    Some(id) => id as i64,
                    None => {
                        s.query_cursors.remove(&handle);
                        -1
                    }
                },
                None => -1,
            }
        },
    )?;

    // nearest(tag_ptr, tag_len, x, y, maxd) -> entity-id or -1. 2D broadphase,
    // host-side (ported verbatim from the recovered `bind_scene::nearest`).
    linker.func_wrap(
        m,
        "nearest",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32, x: f32, y: f32, maxd: f32| -> i64 {
            let tag = read_guest_str(&mut caller, ptr, len);
            let max2 = maxd * maxd;
            let mut best: Option<(u32, f32)> = None;
            for (id, e) in caller.data().store.tagged(&tag) {
                let dx = e.pos[0] - x;
                let dy = e.pos[1] - y;
                let d2 = dx * dx + dy * dy;
                if d2 <= max2 && best.map_or(true, |(_, bd)| d2 < bd) {
                    best = Some((id, d2));
                }
            }
            best.map(|(id, _)| id as i64).unwrap_or(-1)
        },
    )?;

    // move-toward(entity, target, speed) — host does the normalize×speed math,
    // ported verbatim from the recovered `bind_scene::move-toward`.
    linker.func_wrap(
        m,
        "move-toward",
        |mut caller: Caller<'_, HostState>, eid: i64, target: i64, speed: f32| {
            let sp = caller.data().store.entities.get(&(eid as u32)).map(|e| e.pos);
            let tp = caller
                .data()
                .store
                .entities
                .get(&(target as u32))
                .map(|e| e.pos);
            if let (Some(sp), Some(tp)) = (sp, tp) {
                let dx = tp[0] - sp[0];
                let dy = tp[1] - sp[1];
                let len = (dx * dx + dy * dy).sqrt();
                let (vx, vy) = if len > 1e-6 {
                    (dx / len * speed, dy / len * speed)
                } else {
                    (0.0, 0.0)
                };
                if let Some(e) = caller.data_mut().store.entities.get_mut(&(eid as u32)) {
                    e.vel = [vx, vy, 0.0];
                }
            }
        },
    )?;

    Ok(())
}

fn bind_input(linker: &mut Linker<HostState>) -> Result<(), RuntimeError> {
    let m = "kami:engine/input@1.0.0";
    linker.func_wrap(
        m,
        "axis",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> f32 {
            let name = read_guest_str(&mut caller, ptr, len);
            caller.data().axes.get(&name).copied().unwrap_or(0.0)
        },
    )?;
    Ok(())
}

fn bind_random(linker: &mut Linker<HostState>) -> Result<(), RuntimeError> {
    let m = "kami:engine/random@1.0.0";
    // int(n) -> uniform i64 in [0, n); 0 if n <= 0. xorshift64, ported verbatim
    // from the recovered `bind_random::int`.
    linker.func_wrap(m, "int", |mut caller: Caller<'_, HostState>, n: i64| -> i64 {
        if n <= 0 {
            return 0;
        }
        let s = caller.data_mut();
        let mut x = s.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.rng = x;
        (x % (n as u64)) as i64
    })?;
    Ok(())
}

fn bind_time(linker: &mut Linker<HostState>) -> Result<(), RuntimeError> {
    let m = "kami:engine/time@1.0.0";
    linker.func_wrap(m, "tick", |caller: Caller<'_, HostState>| -> i64 {
        caller.data().tick_n
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Names of exports ending in `-tick`, in export-section (definition) order.
/// Hand-parses the WASM export section (id 7) so system execution order
/// matches the guest's own `defsystem` definition order, not whatever
/// `Instance::exports()` iteration happens to yield — ported verbatim from
/// the recovered `ordered_tick_exports` (its own comment explains why this
/// matters: engine export-iteration order isn't guaranteed stable).
fn read_uleb(b: &[u8], mut off: usize) -> (u64, usize) {
    let (mut val, mut shift) = (0u64, 0u32);
    while off < b.len() {
        let byte = b[off];
        off += 1;
        val |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (val, off)
}

fn ordered_tick_exports(wasm: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if wasm.len() < 8 {
        return names;
    }
    let mut i = 8; // skip magic(4) + version(4)
    while i < wasm.len() {
        let id = wasm[i];
        i += 1;
        let (size, ni) = read_uleb(wasm, i);
        i = ni;
        let end = (i + size as usize).min(wasm.len());
        if id == 7 {
            let (count, mut j) = read_uleb(wasm, i);
            for _ in 0..count {
                let (nlen, nj) = read_uleb(wasm, j);
                let e = (nj + nlen as usize).min(wasm.len());
                let name = std::str::from_utf8(&wasm[nj.min(wasm.len())..e])
                    .unwrap_or("")
                    .to_string();
                j = e;
                j += 1; // export kind byte
                let (_idx, jj) = read_uleb(wasm, j);
                j = jj;
                if name.ends_with("-tick") {
                    names.push(name);
                }
            }
            break;
        }
        i = end;
    }
    names
}

pub struct KamiHost {
    engine: Engine,
    store: WasmtimeStore<HostState>,
    instance: Instance,
    systems: Vec<String>,
}

impl KamiHost {
    /// Load `.wasm` bytes, bind every host-import, instantiate. `seed`
    /// initializes the deterministic PRNG.
    pub fn load(wasm: &[u8], seed: u64) -> Result<Self, RuntimeError> {
        let engine = Engine::default();
        let mut linker: Linker<HostState> = Linker::new(&engine);
        bind_scene(&mut linker)?;
        bind_input(&mut linker)?;
        bind_random(&mut linker)?;
        bind_time(&mut linker)?;

        let mut store = WasmtimeStore::new(&engine, HostState::new(seed));
        let module = Module::new(&engine, wasm)?;
        let instance = linker.instantiate(&mut store, &module)?;
        let systems = ordered_tick_exports(wasm);

        Ok(Self { engine: engine.clone(), store, instance, systems })
    }

    pub fn set_axis(&mut self, name: &str, value: f32) {
        self.store.data_mut().set_axis(name, value);
    }

    pub fn call_init(&mut self) -> Result<(), RuntimeError> {
        let f = self
            .instance
            .get_typed_func::<(), i64>(&mut self.store, "init")
            .map_err(|_| RuntimeError::MissingExport("init".into()))?;
        f.call(&mut self.store, ())?;
        Ok(())
    }

    /// Run every `<name>-tick` export once, in definition order, advancing
    /// `tick_n` first (matches the recovered `call_systems`'s ordering:
    /// counters advance, then every system runs against the new tick_n), then
    /// integrate motion. The recovered original kept `integrate` a separate
    /// public method the caller invokes each frame after `call_systems`
    /// (`tests/survivors.rs`: `call_systems` then `integrate`, every loop
    /// iteration) — folded into one call here since nothing in this repo
    /// needs them decoupled, but the motion step itself (fixed-step Euler,
    /// Position += Velocity * dt) is ported verbatim from the recovered
    /// `KamiScriptRuntime::integrate`. Missing this step was caught by this
    /// repo's own tick-loop verification: without it, `move-toward!`-set
    /// velocities never advanced any position, so no ghost ever entered
    /// weapon/contact range in 200 ticks — see README "Verification".
    pub fn tick(&mut self, dt_ms: i64) -> Result<(), RuntimeError> {
        self.store.data_mut().tick_n += 1;
        let systems = self.systems.clone();
        for sys in &systems {
            if let Ok(f) = self
                .instance
                .get_typed_func::<i64, i64>(&mut self.store, sys)
            {
                f.call(&mut self.store, dt_ms)?;
            }
        }
        self.integrate(dt_ms);
        self.store.data_mut().query_cursors.clear();
        Ok(())
    }

    /// Fixed-step Euler integration: advance every entity's position by its
    /// velocity over `dt_ms`. Ported verbatim from the recovered
    /// `KamiScriptRuntime::integrate`.
    pub fn integrate(&mut self, dt_ms: i64) {
        let dt = dt_ms as f32 / 1000.0;
        for e in self.store.data_mut().store.entities.values_mut() {
            e.pos[0] += e.vel[0] * dt;
            e.pos[1] += e.vel[1] * dt;
            e.pos[2] += e.vel[2] * dt;
        }
    }

    pub fn entity_count(&self) -> usize {
        self.store.data().store.entities.len()
    }

    /// Debug-only: (id, tag, pos, vel) for every live entity.
    pub fn debug_dump(&self) -> Vec<(u32, String, [f32; 3], [f32; 3])> {
        let s = &self.store.data().store;
        s.entities
            .iter()
            .map(|(id, e)| {
                (
                    *id,
                    s.tags.get(id).cloned().unwrap_or_default(),
                    e.pos,
                    e.vel,
                )
            })
            .collect()
    }

    pub fn tagged_count(&self, tag: &str) -> usize {
        self.store.data().store.tagged(tag).count()
    }

    pub fn engine_backend(&self) -> &'static str {
        let _ = &self.engine;
        "wasmtime"
    }
}
