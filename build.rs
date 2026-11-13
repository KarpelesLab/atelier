//! Defines three cfgs:
//!
//! - `unixlike`: Unix, or a target whose std offers `std::os::unix` without
//!   being in the `unix` family — fullrust (`x86_64-unknown-linux-fullrust`),
//!   the libc-free static Linux target releases are built for. atelier's
//!   Unix-only code (sockets, process groups, file modes) is gated on it
//!   rather than on `cfg(unix)`, so it is kept in such builds.
//! - `tui`: the `tui` feature is on and the target is one the terminal
//!   layer (`src/tui/term.rs`) knows the terminal structures of. Elsewhere
//!   atelier is built with the line REPL only.
//! - `sandbox`: the `sandbox` feature is on and the target is in the `unix`
//!   family proper, which nixvm needs to mount a host directory in its guest
//!   (it calls libc's `*at` functions, and fullrust has no libc). Elsewhere
//!   the feature compiles to "not available in this build".

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(unixlike)");
    println!("cargo::rustc-check-cfg=cfg(tui)");
    println!("cargo::rustc-check-cfg=cfg(sandbox)");
    // Stock rustc doesn't know the fullrust toolchain's `target_os`.
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"fullrust\"))");

    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let (os, arch) = (var("CARGO_CFG_TARGET_OS"), var("CARGO_CFG_TARGET_ARCH"));
    if std::env::var_os("CARGO_CFG_UNIX").is_some() || os == "fullrust" {
        println!("cargo::rustc-cfg=unixlike");
    }
    if std::env::var_os("CARGO_FEATURE_SANDBOX").is_some()
        && std::env::var_os("CARGO_CFG_UNIX").is_some()
    {
        println!("cargo::rustc-cfg=sandbox");
    }
    // Linux's terminal structures are the same on these architectures
    // (not on PowerPC, MIPS or SPARC).
    let generic = ["x86_64", "aarch64", "x86", "arm", "riscv64"].contains(&arch.as_str());
    let terminal = os == "macos" || (generic && (os == "linux" || os == "fullrust"));
    if std::env::var_os("CARGO_FEATURE_TUI").is_some() && terminal {
        println!("cargo::rustc-cfg=tui");
    }
}
