fn main() {
    println!("cargo::rustc-check-cfg=cfg(loom)");
    // the loom we are built against has `AtomicPtr::load_consume()`, see `ord::active_load()`
    println!("cargo::rustc-check-cfg=cfg(local_rcu_loom_consume)");
    // one weakened ordering or missing fence each, see `ord` in src/lib.rs and mutants.sh
    println!(
        "cargo::rustc-check-cfg=cfg(local_rcu_mutant, values({}))",
        [
            "no_reader_fence",
            "no_writer_fence",
            "enter_store_relaxed",
            "active_load_relaxed",
            "leave_store_relaxed",
            "drop_store_relaxed",
            "publish_store_relaxed",
            "scan_load_relaxed",
            "collect_load_relaxed",
            "held_load_relaxed",
            "no_hold_nothing",
        ]
        .map(|name| format!("\"{name}\""))
        .join(", ")
    );
}
