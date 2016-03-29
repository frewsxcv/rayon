//! Unwinding recovery is an unstable feature and hence only available
//! on the nightly compiler right now.

use std::any::Any;
use std::thread;

/// Executes `f` and captures any panic, translating that panic into a
/// `Err` result. The assumption is that any panic will be propagated
/// later with `resume_unwinding`, and hence `f` can be treated as
/// exception safe.
#[cfg(feature = "nightly")]
pub fn halt_unwinding<F,R>(func: F) -> thread::Result<R>
    where F: FnOnce() -> R
{
    use std::panic::{self, AssertRecoverSafe};
    panic::recover(AssertRecoverSafe(func))
}

#[cfg(feature = "nightly")]
pub fn resume_unwinding(payload: Box<Any + Send>) -> ! {
    use std::panic;
    panic::propagate(payload)
}

#[cfg(not(feature = "nightly"))]
pub fn halt_unwinding<F,R>(func: F) -> thread::Result<R>
    where F: FnOnce() -> R
{
    use libc;
    use std::mem;

    // If you're not on nightly, we can't actually halt unwinding, so
    // just translate a panic into an **abort**.
    let abort = AbortIfPanic;
    let result = func();
    mem::forget(abort);
    return Ok(result);

    struct AbortIfPanic;

    impl Drop for AbortIfPanic {
        fn drop(&mut self) {
            unsafe {
                libc::abort();
            }
        }
    }
}

#[cfg(not(feature = "nightly"))]
pub fn resume_unwinding(_payload: Box<Any + Send>) -> ! {
    panic!("rayon: panic occurred")
}

