//! `wasmi` (pure interpretation, no JIT — the iOS/PS5/Switch-safe path) twin
//! of `KamiHost` in `lib.rs`. Same public API surface, same `HostState`/
//! `EcsStore`, same 14 host-import *semantics* — only the WASM engine
//! differs. Written to let `tests::wasmtime_wasmi_parity` run the identical
//! compiled module through both and assert identical observable state
//! (positions/entity-counts) after N ticks, per the original recovered
//! design's own documented contract ("two WASM backends, one binding
//! codebase... deterministic: both backends produce bit-identical runs").

use crate::{HostState, RuntimeError, ordered_tick_exports};
use wasmi::{Caller, Engine, Extern, Instance, Linker, Module, Store as WasmiStore};

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

fn bind_scene(linker: &mut Linker<HostState>) -> Result<(), RuntimeError> {
    let m = "kami:engine/scene@1.0.0";

    linker.func_wrap(m, "spawn", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let tag = read_guest_str(&mut caller, ptr, len);
        caller.data_mut().store.spawn(&tag) as i64
    })?;

    linker.func_wrap(m, "despawn", |mut caller: Caller<'_, HostState>, eid: i64| {
        caller.data_mut().store.despawn(eid as u32);
    })?;

    linker.func_wrap(
        m, "set-position",
        |mut caller: Caller<'_, HostState>, eid: i64, x: f32, y: f32, z: f32| {
            if let Some(e) = caller.data_mut().store.entities.get_mut(&(eid as u32)) {
                e.pos = [x, y, z];
            }
        },
    )?;

    linker.func_wrap(
        m, "set-velocity",
        |mut caller: Caller<'_, HostState>, eid: i64, vx: f32, vy: f32, vz: f32| {
            if let Some(e) = caller.data_mut().store.entities.get_mut(&(eid as u32)) {
                e.vel = [vx, vy, vz];
            }
        },
    )?;

    linker.func_wrap(m, "get-x", |caller: Caller<'_, HostState>, eid: i64| -> f32 {
        caller.data().store.entities.get(&(eid as u32)).map(|e| e.pos[0]).unwrap_or(0.0)
    })?;

    linker.func_wrap(m, "get-y", |caller: Caller<'_, HostState>, eid: i64| -> f32 {
        caller.data().store.entities.get(&(eid as u32)).map(|e| e.pos[1]).unwrap_or(0.0)
    })?;

    linker.func_wrap(
        m, "count-tagged",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            let tag = read_guest_str(&mut caller, ptr, len);
            caller.data().store.tagged(&tag).count() as i64
        },
    )?;

    linker.func_wrap(
        m, "query-begin",
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

    linker.func_wrap(m, "query-next", |mut caller: Caller<'_, HostState>, handle: i64| -> i64 {
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
    })?;

    linker.func_wrap(
        m, "nearest",
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

    linker.func_wrap(
        m, "move-toward",
        |mut caller: Caller<'_, HostState>, eid: i64, target: i64, speed: f32| {
            let sp = caller.data().store.entities.get(&(eid as u32)).map(|e| e.pos);
            let tp = caller.data().store.entities.get(&(target as u32)).map(|e| e.pos);
            if let (Some(sp), Some(tp)) = (sp, tp) {
                let dx = tp[0] - sp[0];
                let dy = tp[1] - sp[1];
                let len = (dx * dx + dy * dy).sqrt();
                let (vx, vy) = if len > 1e-6 { (dx / len * speed, dy / len * speed) } else { (0.0, 0.0) };
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
    linker.func_wrap(m, "axis", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> f32 {
        let name = read_guest_str(&mut caller, ptr, len);
        caller.data().axes.get(&name).copied().unwrap_or(0.0)
    })?;
    Ok(())
}

fn bind_random(linker: &mut Linker<HostState>) -> Result<(), RuntimeError> {
    let m = "kami:engine/random@1.0.0";
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
    linker.func_wrap(m, "tick", |caller: Caller<'_, HostState>| -> i64 { caller.data().tick_n })?;
    Ok(())
}

/// `wasmi` twin of [`crate::KamiHost`] — identical public API, no-JIT engine.
pub struct KamiHostWasmi {
    store: WasmiStore<HostState>,
    instance: Instance,
    systems: Vec<String>,
}

impl KamiHostWasmi {
    pub fn load(wasm: &[u8], seed: u64) -> Result<Self, RuntimeError> {
        let engine = Engine::default();
        let mut linker: Linker<HostState> = Linker::new(&engine);
        bind_scene(&mut linker)?;
        bind_input(&mut linker)?;
        bind_random(&mut linker)?;
        bind_time(&mut linker)?;

        let mut store = WasmiStore::new(&engine, HostState::new(seed));
        let module = Module::new(&engine, wasm)?;
        let instance = linker.instantiate_and_start(&mut store, &module)?;
        let systems = ordered_tick_exports(wasm);

        Ok(Self { store, instance, systems })
    }

    pub fn set_axis(&mut self, name: &str, value: f32) {
        self.store.data_mut().set_axis(name, value);
    }

    pub fn call_init(&mut self) -> Result<(), RuntimeError> {
        let f = self
            .instance
            .get_typed_func::<(), i64>(&self.store, "init")
            .map_err(|_| RuntimeError::MissingExport("init".into()))?;
        f.call(&mut self.store, ())?;
        Ok(())
    }

    pub fn tick(&mut self, dt_ms: i64) -> Result<(), RuntimeError> {
        self.store.data_mut().tick_n += 1;
        let systems = self.systems.clone();
        for sys in &systems {
            if let Ok(f) = self.instance.get_typed_func::<i64, i64>(&self.store, sys) {
                f.call(&mut self.store, dt_ms)?;
            }
        }
        self.integrate(dt_ms);
        self.store.data_mut().query_cursors.clear();
        Ok(())
    }

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

    pub fn debug_dump(&self) -> Vec<(u32, String, [f32; 3], [f32; 3])> {
        let s = &self.store.data().store;
        s.entities
            .iter()
            .map(|(id, e)| (*id, s.tags.get(id).cloned().unwrap_or_default(), e.pos, e.vel))
            .collect()
    }

    pub fn tagged_count(&self, tag: &str) -> usize {
        self.store.data().store.tagged(tag).count()
    }

    pub fn engine_backend(&self) -> &'static str {
        "wasmi"
    }
}
