//! The capturing `console` host object for the `node` tool.
//!
//! `console.log` / `console.error` stringify their arguments (space-joined) and
//! append a line to a shared [`String`] buffer. The buffer is returned so the
//! tool can hand the captured output back as its result. Closures are
//! `FnMut + 'static` and single-threaded, so `Rc<RefCell<_>>` is enough.

use std::cell::RefCell;
use std::rc::Rc;

use kataan::{Ctx, Interp, NanBox};

/// Space-join the string form of every argument.
fn join_args(cx: &mut Ctx, args: &[NanBox]) -> Result<String, NanBox> {
    let mut parts = Vec::with_capacity(args.len());
    for a in args {
        parts.push(cx.to_string(*a)?);
    }
    Ok(parts.join(" "))
}

/// Register `__atelier_console_log` / `__atelier_console_error` and return the
/// shared buffer they write into. The bootstrap program assembles them into
/// `globalThis.console`.
pub fn install(interp: &mut Interp) -> Rc<RefCell<String>> {
    let buf = Rc::new(RefCell::new(String::new()));

    let log_buf = Rc::clone(&buf);
    interp.register_global_fn("__atelier_console_log", 0, move |cx, _this, args| {
        let line = join_args(cx, args)?;
        let mut b = log_buf.borrow_mut();
        b.push_str(&line);
        b.push('\n');
        Ok(cx.undefined())
    });

    let err_buf = Rc::clone(&buf);
    interp.register_global_fn("__atelier_console_error", 0, move |cx, _this, args| {
        let line = join_args(cx, args)?;
        let mut b = err_buf.borrow_mut();
        b.push_str("error: ");
        b.push_str(&line);
        b.push('\n');
        Ok(cx.undefined())
    });

    buf
}
