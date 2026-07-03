//! End-to-end: isekai-network's real `games/01-netsurvivors/logic.clj`,
//! compiled by `kotoba-lang/engine` to real `.wasm` bytes, loaded and driven
//! for many ticks on this host. Ported in spirit from the recovered
//! `kami-engine@a8368f9c0d784dbc9d11e8fa8f407aa95c7ce4fa:kami-script-runtime/tests/survivors.rs`
//! (same shape: init, run N ticks, assert real state evolved).

use kami_script_runtime_rs::KamiHost;

const WASM: &[u8] =
    include_bytes!("fixtures/isekai-network-01-netsurvivors.wasm");

#[test]
fn survivors_core_loop_evolves() {
    let mut host = KamiHost::load(WASM, 7).expect("load+instantiate");
    host.call_init().expect("call init");

    for _ in 0..300 {
        host.tick(16).expect("tick");
    }

    assert_eq!(host.tagged_count("shiro-pico"), 1, "exactly one duo");
    assert!(host.tagged_count("ghost") > 0, "wave spawning produced ghosts");
    assert!(
        host.tagged_count("ghost") < 120,
        "alive count stays under max-alive"
    );

    // At least one ghost must have moved off its spawn point by now (the
    // regression this repo's README documents: before kotoba-lang/engine#2,
    // ghosts spawned via an inline negative f32 literal never moved at all).
    let moved = host
        .debug_dump()
        .into_iter()
        .filter(|(_, tag, ..)| tag == "ghost")
        .any(|(_, _, pos, _)| pos[0] != 0.0 || pos[1] != 0.0);
    assert!(moved, "at least one ghost should have moved from (0,0)");
}

#[test]
fn weapon_culls_a_synthetic_scene() {
    // A minimal, hand-authored scene (not compiled from isekai-network) to
    // isolate the weapon/nearest/despawn path independent of spawn timing.
    let src = r#"
        (defn init []
          (let [p (spawn-entity "player")]
            (set-position! p (f32 0.0) (f32 0.0) (f32 0.0)))
          (spawn-entity "enemy")
          (spawn-entity "enemy")
          (spawn-entity "enemy"))
        (defsystem weapon [dt]
          (let [hit (nearest-tagged "enemy" (f32 0.0) (f32 0.0) (f32 50.0))]
            (when (not= hit -1)
              (despawn-entity hit))))
    "#;
    // Compile via kotoba-lang/engine at test time so this stays a real,
    // independent end-to-end check (not just replaying a checked-in fixture).
    let out = std::process::Command::new("clojure")
        .args(["-M", "-e", &format!(
            r#"(require '[kotoba.engine-clj.codegen :as c] '[kotoba.engine-clj.ast :as a] '[kotoba.engine-clj.wasm-bytes :as w])
               (let [ir (c/compile (a/parse-program-str {src:?}))
                     bs (w/emit-module-bytes ir)]
                 (print (apply str (map char bs))))"#
        )])
        .current_dir(std::env::var("ENGINE_REPO_PATH").unwrap_or_else(|_| ".".into()))
        .output();

    // This test only runs meaningfully if ENGINE_REPO_PATH points at a
    // kotoba-lang/engine checkout with clojure on PATH -- skip gracefully
    // otherwise rather than failing CI on an environment gap.
    let Ok(out) = out else {
        eprintln!("skipping weapon_culls_a_synthetic_scene: clojure not runnable here");
        return;
    };
    if !out.status.success() {
        eprintln!("skipping weapon_culls_a_synthetic_scene: compile step failed (needs ENGINE_REPO_PATH set to a kotoba-lang/engine checkout)");
        return;
    }
    let wasm: Vec<u8> = out.stdout.iter().map(|&b| b).collect();
    if wasm.len() < 8 {
        eprintln!("skipping weapon_culls_a_synthetic_scene: no wasm produced");
        return;
    }

    let mut host = KamiHost::load(&wasm, 1).expect("load+instantiate");
    host.call_init().expect("call init");
    assert_eq!(host.tagged_count("enemy"), 3);
    host.tick(16).expect("tick");
    assert_eq!(host.tagged_count("enemy"), 2, "one culled per fire");
}
