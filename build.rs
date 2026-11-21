fn main() {
    println!("cargo::rustc-check-cfg=cfg(loom)");
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
        ]
        .map(|name| format!("\"{name}\""))
        .join(", ")
    );
}
