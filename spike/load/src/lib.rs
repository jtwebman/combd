//! Loading a compiled module's build into a running node. With combd-mem,
//! this is combd's `unsafe` code outside the AOT tier itself. What it relies on:
//! - The build is what `combd-spike aot-build` makes: the module's generated
//!   Rust (safe Rust, `forbid(unsafe_code)`) with combd-aot, as a library
//!   that exports `combd_pin` and `combd_module` by the C ABI.
//! - Its pin is ours (the same rustc, the same combd-aot sources, the same
//!   target), so every type that crosses (`Compiled`, the runtime's `Ctx`,
//!   `Mem`, `Trap`, a `Box<dyn Host>`) has the same layout on both sides.
//!   The pin is read through the C ABI first; nothing else is touched unless
//!   it matches.
//! - Whoever put the file there is trusted: the node checks a builder's
//!   signature before it calls `open`.
//!
//! A loaded build stays loaded (and its `Compiled` lives) for as long as
//! the process does.

use combd_aot::Compiled;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Load the build at `path`: its module, if it was made for this binary.
pub fn open(path: &Path) -> Result<&'static Compiled, String> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| "a path with a nul in it")?;
    // SAFETY: a nul-terminated path; dlopen runs the library's initializers,
    // which a build made by aot-build has none of its own (Rust's std only).
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        return Err(format!("{}: {}", path.display(), last_error()));
    }
    let close = |why: String| {
        // SAFETY: the handle dlopen gave, closed once, nothing from it kept.
        unsafe { libc::dlclose(h) };
        Err(why)
    };
    // SAFETY: looking a name up in a handle we hold.
    let pin = unsafe { libc::dlsym(h, c"combd_pin".as_ptr()) };
    let module = unsafe { libc::dlsym(h, c"combd_module".as_ptr()) };
    if pin.is_null() || module.is_null() {
        return close(format!("{}: not a combd build (no combd_pin or combd_module)", path.display()));
    }
    // SAFETY: `combd_pin` is `extern "C" fn() -> *const c_char` in every
    // build aot-build makes, returning a nul-terminated string it keeps.
    let pin: extern "C" fn() -> *const libc::c_char = unsafe { std::mem::transmute(pin) };
    let theirs = unsafe { CStr::from_ptr(pin()) }.to_string_lossy().into_owned();
    if theirs.as_bytes() != combd_aot::PIN.as_bytes() {
        return close(format!("{}: built with {theirs:?}, this node is {:?}", path.display(), combd_aot::PIN));
    }
    // SAFETY: the pins match, so `combd_module` is `extern "C" fn() ->
    // *const Compiled` with our `Compiled`, pointing at a static in the
    // library, which stays loaded from here on.
    let module: extern "C" fn() -> *const Compiled = unsafe { std::mem::transmute(module) };
    Ok(unsafe { &*module() })
}

fn last_error() -> String {
    // SAFETY: dlerror's string is valid until the next dl* call on this thread.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown error".into()
    } else {
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}
