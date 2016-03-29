use job::{Job, JobRef};
use std::any::Any;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicUsize, AtomicPtr, Ordering};
use std::sync::{Condvar, Mutex};
use thread_pool::{self, WorkerThread};
use unwind;

#[cfg(test)]
mod test;

pub struct Scope<'scope> {
    counter: AtomicUsize,
    panic: AtomicPtr<Box<Any + Send + 'static>>,
    mutex: Mutex<()>,
    cvar: Condvar,
    marker: PhantomData<fn(&'scope ())>,
}

/// Create a "fork-join" scope.
pub fn scope<'scope, OP, R>(op: OP) -> R
    where OP: for<'s> FnOnce(&'s Scope<'scope>) -> R
{
    let scope = Scope {
        counter: AtomicUsize::new(1),
        panic: AtomicPtr::new(ptr::null_mut()),
        mutex: Mutex::new(()),
        cvar: Condvar::new(),
        marker: PhantomData,
    };
    let result = op(&scope);
    scope.job_completed_ok(); // `op` counts as a job
    scope.block_till_jobs_complete();
    result
}

impl<'scope> Scope<'scope> {
    /// Injects a job into the current fork-join scope. This job will
    /// execute sometime before the fork-join scope completes.
    pub fn spawn<BODY>(&self, body: BODY)
        where BODY: FnOnce(&Scope<'scope>) + 'scope
    {
        unsafe {
            let old_value = self.counter.fetch_add(1, Ordering::SeqCst);
            assert!(old_value > 0); // scope can't have completed yet
            let job_ref = Box::new(HeapJob::new(self, body)).as_job_ref();
            let worker_thread = WorkerThread::current();
            if !worker_thread.is_null() {
                let worker_thread = &*worker_thread;
                let spawn_count = worker_thread.spawn_count();
                spawn_count.set(spawn_count.get() + 1);
                worker_thread.push(job_ref);
            } else {
                thread_pool::get_registry().inject(&[job_ref]);
            }
        }
    }

    fn job_panicked(&self, err: Box<Any + Send + 'static>) {
        // capture the first error we see, free the rest
        let nil = ptr::null_mut();
        let mut err = Box::new(err); // box up the fat ptr
        if self.panic.compare_and_swap(nil, &mut *err, Ordering::SeqCst).is_null() {
            mem::forget(err); // ownership now transferred into self.panic
        }

        self.job_completed_ok()
    }

    fn job_completed_ok(&self) {
        let old_value = self.counter.fetch_sub(1, Ordering::Release);
        if old_value == 1 {
            // Important: grab the lock here to avoid a data race with
            // the `block_till_jobs_complete` code. Consider what could
            // otherwise happen:
            //
            // ```
            //    Us          Them
            //              Acquire lock
            //              Read counter: 1
            // Dec counter
            // Notify all
            //              Wait on cvar
            // ```
            //
            // By holding the lock, we ensure that the "read counter"
            // and "wait on cvar" occur atomically with respect to the
            // notify.
            let _guard = self.mutex.lock().unwrap();
            self.cvar.notify_all();
        }
    }

    fn block_till_jobs_complete(&self) {
        // wait for job counter to reach 0:
        //
        // FIXME -- if on a worker thread, we should be helping here
        let mut guard = self.mutex.lock().unwrap();
        while self.counter.load(Ordering::Acquire) > 0 {
            guard = self.cvar.wait(guard).unwrap();
        }

        // propagate panic, if any occurred; at this point, all
        // outstanding jobs have completed, so we can use a relaxed
        // ordering:
        let panic = self.panic.swap(ptr::null_mut(), Ordering::Relaxed);
        if !panic.is_null() {
            unsafe {
                let value: Box<Box<Any + Send + 'static>> = mem::transmute(panic);
                unwind::resume_unwinding(*value);
            }
        }
    }
}

struct HeapJob<'scope, BODY>
    where BODY: FnOnce(&Scope<'scope>) + 'scope,
{
    scope: *const Scope<'scope>,
    func: UnsafeCell<Option<BODY>>,
}

impl<'scope, BODY> HeapJob<'scope, BODY>
    where BODY: FnOnce(&Scope<'scope>) + 'scope
{
    fn new(scope: *const Scope<'scope>, func: BODY) -> Self {
        HeapJob {
            scope: scope,
            func: UnsafeCell::new(Some(func))
        }
    }

    unsafe fn as_job_ref(self: Box<Self>) -> JobRef {
        let job_ref: *const Self = mem::transmute(self);
        let job_ref: *const (Job + 'scope) = job_ref;
        let job_ref: *const (Job + 'static) = mem::transmute(job_ref);
        JobRef(job_ref)
    }

    /// We have to maintain an invariant that we pop off any work
    /// that we pushed onto the local thread deque. In other words,
    /// if no thieves are at play, then the height of the local
    /// deque must be the same when we enter and exit. Otherwise,
    /// we get into trouble composing with the main `join` API,
    /// which assumes that -- after it executes the first closure
    /// -- the top-most thing on the stack is the second closure.
    unsafe fn pop_jobs(worker_thread: &WorkerThread, start_count: usize) {
        let spawn_count = worker_thread.spawn_count();
        let current_count = spawn_count.get();
        for _ in start_count .. current_count {
            if let Some(job_ref) = worker_thread.pop() {
                (*job_ref.0).execute();
            }
        }
        spawn_count.set(start_count);
    }
}

impl<'scope, BODY> Job for HeapJob<'scope, BODY>
    where BODY: FnOnce(&Scope<'scope>) + 'scope
{
    unsafe fn execute(&self) {
        let scope = &*self.scope;
        let worker_thread = &*WorkerThread::current();
        let start_count = worker_thread.spawn_count().get();

        // Recover-safe is ok because we propagate the panic.
        let func = (*self.func.get()).take().unwrap();
        match unwind::halt_unwinding(|| func(&*scope)) {
            Ok(()) => { (*scope).job_completed_ok(); }
            Err(err) => { (*scope).job_panicked(err); }
        }

        Self::pop_jobs(worker_thread, start_count);
    }

    unsafe fn abort(&self) {
        (*self.scope).job_completed_ok();
    }
}


