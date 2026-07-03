//! Minimal CLI: load a `.wasm` module, call `init`, run N ticks, report
//! entity counts. `kami-host <path.wasm> [ticks] [seed]`.
use kami_script_runtime_rs::KamiHost;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: kami-host <path.wasm> [ticks] [seed]");
    let ticks: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100);
    let seed: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(7);

    let wasm = std::fs::read(&path).expect("read wasm file");
    let mut host = KamiHost::load(&wasm, seed).expect("load+instantiate");
    host.call_init().expect("call init");

    for i in 0..ticks {
        host.tick(16).expect("tick");
        if i % 20 == 0 || i == ticks - 1 {
            println!(
                "tick {:>4}: entities={:>4} shiro-pico={} ghost={} beat-spark={}",
                i,
                host.entity_count(),
                host.tagged_count("shiro-pico"),
                host.tagged_count("ghost"),
                host.tagged_count("beat-spark"),
            );
            if std::env::var("DEBUG_POS").is_ok() {
                for (id, tag, pos, vel) in host.debug_dump() {
                    println!(
                        "  #{id:>3} {tag:<12} pos=({:>8.2},{:>8.2}) vel=({:>7.2},{:>7.2})",
                        pos[0], pos[1], vel[0], vel[1]
                    );
                }
            }
        }
    }
}
