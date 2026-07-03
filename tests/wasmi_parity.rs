//! Golden-frame parity: the SAME compiled isekai-network module, run for the
//! SAME N ticks under both backends, must observe identical entity state.
//! Ported intent from the original recovered design's own documented
//! contract ("two WASM backends, one binding codebase... deterministic:
//! both backends produce bit-identical runs").
use kami_script_runtime_rs::{KamiHost, KamiHostWasmi};

#[test]
fn wasmtime_and_wasmi_agree_after_200_ticks() {
    let wasm = std::fs::read("tests/fixtures/isekai-network-current.wasm")
        .expect("read fixture wasm");

    let mut wt = KamiHost::load(&wasm, 42).expect("load wasmtime");
    wt.call_init().expect("init wasmtime");
    let mut wi = KamiHostWasmi::load(&wasm, 42).expect("load wasmi");
    wi.call_init().expect("init wasmi");

    assert_eq!(wt.engine_backend(), "wasmtime");
    assert_eq!(wi.engine_backend(), "wasmi");

    for _ in 0..200 {
        wt.tick(16).expect("tick wasmtime");
        wi.tick(16).expect("tick wasmi");
    }

    assert_eq!(wt.entity_count(), wi.entity_count(), "entity_count diverged");
    assert_eq!(
        wt.tagged_count("ghost"), wi.tagged_count("ghost"),
        "ghost count diverged"
    );
    assert_eq!(
        wt.tagged_count("shiro-pico"), wi.tagged_count("shiro-pico"),
        "shiro-pico count diverged"
    );

    // per-entity position parity: sort both dumps by id, compare bit-for-bit.
    let mut a = wt.debug_dump();
    let mut b = wi.debug_dump();
    a.sort_by_key(|(id, ..)| *id);
    b.sort_by_key(|(id, ..)| *id);
    assert_eq!(a.len(), b.len(), "entity count mismatch in dumps");
    for ((id_a, tag_a, pos_a, vel_a), (id_b, tag_b, pos_b, vel_b)) in a.iter().zip(b.iter()) {
        assert_eq!(id_a, id_b, "entity id order diverged");
        assert_eq!(tag_a, tag_b, "tag diverged for entity {id_a}");
        assert_eq!(pos_a, pos_b, "position diverged for entity {id_a} ({tag_a})");
        assert_eq!(vel_a, vel_b, "velocity diverged for entity {id_a} ({tag_a})");
    }
}
