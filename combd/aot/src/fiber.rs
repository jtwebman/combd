//! Pausable compiled cells. A call runs on its own small native stack (a
//! corosensei coroutine). When its CPU slice runs out, generated code calls
//! `Ctx::out_of_fuel`, which suspends the coroutine back to the scheduler, so
//! other cells get the core, BEAM-style. The next `resume` continues exactly
//! where it stopped.
//!
//! Unsafe code lives here, in `guard.rs`, and in `trap.c` only.

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use std::cell::Cell;
use std::ffi::c_void;

/// Native stack per running call. Pages are mapped lazily, so a paused cell
/// only costs the pages it touched. Must stay above `rt::STACK_BUDGET`.
const STACK: usize = 2 << 20;

thread_local! {
    /// The coroutine running on this thread right now, if any.
    static YIELDER: Cell<*const Yielder<(), ()>> = const { Cell::new(std::ptr::null()) };
}

extern "C" {
    fn combd_trap_take(jmp: *mut *mut c_void, lo: *mut *mut c_void, hi: *mut *mut c_void);
    fn combd_trap_put(jmp: *mut c_void, lo: *mut c_void, hi: *mut c_void);
}

/// Suspend the running coroutine. Returns false if there is none (the call
/// isn't running on a fiber), in which case the caller must stop instead.
pub fn yield_now() -> bool {
    let y = YIELDER.with(|c| c.get());
    if y.is_null() {
        return false;
    }
    let (mut jmp, mut lo, mut hi) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
    unsafe { combd_trap_take(&mut jmp, &mut lo, &mut hi) };
    YIELDER.with(|c| c.set(std::ptr::null()));
    // Safe: `y` points at this coroutine's own yielder, alive while it runs.
    unsafe { (*y).suspend(()) };
    YIELDER.with(|c| c.set(y));
    unsafe { combd_trap_put(jmp, lo, hi) };
    true
}

/// One call in flight on its own stack. `T` is whatever the call returns.
pub struct Fiber<T: 'static> {
    co: Option<Coroutine<(), (), T, DefaultStack>>,
}

pub enum Step<T> {
    Paused,
    Done(T),
}

impl<T: 'static> Fiber<T> {
    pub fn start(spare: Option<DefaultStack>, f: impl FnOnce() -> T + 'static) -> Fiber<T> {
        let stack = spare.unwrap_or_else(|| DefaultStack::new(STACK).expect("could not map a fiber stack"));
        let co = Coroutine::with_stack(stack, move |y: &Yielder<(), ()>, ()| {
            YIELDER.with(|c| c.set(y as *const _));
            let r = f();
            YIELDER.with(|c| c.set(std::ptr::null()));
            r
        });
        Fiber { co: Some(co) }
    }

    /// Run until the call pauses or ends. On `Done`, the stack comes back for reuse.
    pub fn resume(&mut self) -> (Step<T>, Option<DefaultStack>) {
        let co = self.co.as_mut().expect("resumed a finished fiber");
        match co.resume(()) {
            CoroutineResult::Yield(()) => (Step::Paused, None),
            CoroutineResult::Return(r) => {
                let stack = self.co.take().unwrap().into_stack();
                (Step::Done(r), Some(stack))
            }
        }
    }
}
