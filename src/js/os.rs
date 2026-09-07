//! Host facts exposed as the `os` global for the `node` tool.
//!
//! Unlike `fs`, these are pure, static host facts — no filesystem or network
//! access — so they're registered unconditionally (not gated by `network`)
//! and don't affect the tool's confinement or approval requirement. Each
//! fact is a zero-arg `__atelier_os_*` global function; the bootstrap
//! program (`runtime::BOOTSTRAP`) assembles them into `globalThis.os`,
//! following the same GC-safety pattern as `fs`/`console`: nothing is held
//! as a `NanBox` on the Rust side across runs.

use kataan::{Ctx, Interp};

/// Map `std::env::consts::OS` to Node's `os.platform()` convention.
fn platform_str() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Map `std::env::consts::OS` to Node's `os.type()` convention (the
/// underlying kernel/OS name, e.g. `uname -s` on Unix).
fn type_str() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "netbsd" => "NetBSD",
        other => other,
    }
}

/// Register the `__atelier_os_*` global functions on `interp`. The bootstrap
/// program assembles them (plus the constant `EOL`) into `globalThis.os`.
pub fn install(interp: &mut Interp) {
    interp.register_global_fn("__atelier_os_platform", 0, |cx: &mut Ctx, _this, _args| {
        Ok(cx.string(platform_str()))
    });
    interp.register_global_fn("__atelier_os_arch", 0, |cx: &mut Ctx, _this, _args| {
        Ok(cx.string(std::env::consts::ARCH))
    });
    interp.register_global_fn("__atelier_os_type", 0, |cx: &mut Ctx, _this, _args| {
        Ok(cx.string(type_str()))
    });
}
