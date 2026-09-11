use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=augur_plugin_stage_a_a2.def");

    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let definition_file = manifest_dir.join("augur_plugin_stage_a_a2.def");
    println!(
        "cargo:rustc-cdylib-link-arg=/DEF:{}",
        definition_file.display()
    );
}
