//! Builds `-uidisplay`'s preload hook (uihook/uihook.rs) for the target, as a
//! shared object xwlbridge embeds and writes out at run time. One file, no
//! dependencies: rustc is called directly, with the linker cargo would use.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=uihook/uihook.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let mut cmd = Command::new(rustc);
    cmd.args([
        "--crate-type=cdylib",
        "--crate-name=xwlbridge_uihook",
        "--edition=2024",
        "-Copt-level=s",
        "-Cpanic=abort",
        "-Cstrip=symbols",
        "--target",
        &target,
        "-o",
    ])
    .arg(out.join("libxwlbridge_uihook.so"))
    .arg("uihook/uihook.rs");
    let linker_var = format!("CARGO_TARGET_{}_LINKER", target.to_uppercase().replace(['-', '.'], "_"));
    println!("cargo:rerun-if-env-changed={linker_var}");
    if let Some(linker) = env::var_os(&linker_var) {
        let mut arg = std::ffi::OsString::from("-Clinker=");
        arg.push(linker);
        cmd.arg(arg);
    }
    // a wrapper (sccache and the like) is for crates cargo builds, not this
    cmd.env_remove("RUSTC_WRAPPER").env_remove("RUSTC_WORKSPACE_WRAPPER");
    let status = cmd.status().expect("run rustc for uihook");
    assert!(status.success(), "building uihook/uihook.rs failed");
}
