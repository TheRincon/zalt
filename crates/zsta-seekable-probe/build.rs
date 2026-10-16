//! Compiles the reference XXH3-128 (`c/`), the oracle `zalt-archive`'s own
//! implementation is tested against. Test-only, like the rest of this crate.

fn main() {
    println!("cargo:rerun-if-changed=c/xxh3_ref.c");
    println!("cargo:rerun-if-changed=c/xxhash.h");
    cc::Build::new()
        .file("c/xxh3_ref.c")
        .include("c")
        .opt_level(2)
        .compile("zsta_probe_xxh3");
}
