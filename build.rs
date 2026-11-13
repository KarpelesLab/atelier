//! Defines `cfg(unixlike)`: Unix, or a target whose std offers
//! `std::os::unix` without being in the `unix` family — fullrust
//! (`x86_64-unknown-linux-fullrust`), the libc-free static Linux target.
//! atelier's Unix-only code (sockets, process groups, file modes) is gated
//! on it rather than on `cfg(unix)`, so it is kept in such builds.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(unixlike)");
    let fullrust = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "fullrust");
    if std::env::var_os("CARGO_CFG_UNIX").is_some() || fullrust {
        println!("cargo::rustc-cfg=unixlike");
    }
}
