//! Calls that wait on the host, for the lite build (bounds checks, no guard
//! pages). A call of a module that imports a waiting host call (Postgres,
//! `call`) runs on its own small native stack (a corosensei coroutine), so
//! the host call can pause it while the event loop does the work, and the
//! next `resume` continues exactly where it stopped. Cells there never pause
//! for time: only these calls do. The guard build has its own (combd-aot's
//! `fiber`, which also carries the trap handler's state across a pause).

use corosensei::{Coroutine, CoroutineResult, Yielder};
use std::cell::Cell;

pub use corosensei::stack::DefaultStack;

/// Native stack per running call. Pages are mapped lazily, so a paused cell
/// only costs the pages it touched. Must stay above combd-aot's
/// `rt::STACK_BUDGET` (1 MiB), the most generated code may use.
const STACK: usize = 2 << 20;

thread_local! {
    /// The coroutine running on this thread right now, if any.
    static YIELDER: Cell<*const Yielder<(), ()>> = const { Cell::new(std::ptr::null()) };
}

/// Suspend the running coroutine. Returns false if there is none (the call
/// isn't running on a fiber), in which case the caller must stop instead.
pub fn yield_now() -> bool {
    let y = YIELDER.with(|c| c.get());
    if y.is_null() {
        return false;
    }
    // Cleared while paused: another fiber may run on this thread meanwhile.
    YIELDER.with(|c| c.set(std::ptr::null()));
    // SAFETY: `y` points at this coroutine's own yielder, alive while it runs,
    // and only this coroutine (which is running: it called us) set it.
    unsafe { (*y).suspend(()) };
    YIELDER.with(|c| c.set(y));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two fibers interleaved on one thread each pause in their own place.
    #[test]
    fn pauses_and_resumes() {
        let mut a = Fiber::start(None, || {
            assert!(yield_now());
            assert!(yield_now());
            1
        });
        let mut b = Fiber::start(None, || {
            assert!(yield_now());
            2
        });
        assert!(matches!(a.resume().0, Step::Paused));
        assert!(matches!(b.resume().0, Step::Paused));
        assert!(matches!(a.resume().0, Step::Paused));
        let (Step::Done(2), Some(spare)) = b.resume() else { panic!("b") };
        assert!(matches!(a.resume().0, Step::Done(1)));
        assert!(!yield_now(), "not on a fiber");
        let mut c = Fiber::start(Some(spare), || 3);
        assert!(matches!(c.resume().0, Step::Done(3)));
    }
}
