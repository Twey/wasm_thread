pub use std::thread::{Result, Thread};
use futures::FutureExt as _;
use std::{
    cell::UnsafeCell,
    fmt,
    future::Future,
    marker::PhantomData,
    mem,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
        Mutex,
    },
};

// use scoped::ScopeData;
// pub use scoped::{scope, Scope, ScopedJoinHandle};
use signal::Signal;
use utils::SpinLockMutex;
pub use utils::{available_parallelism, get_wasm_bindgen_shim_script_path, is_web_worker_thread};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::js_sys;
use web_sys::{DedicatedWorkerGlobalScope, Worker, WorkerOptions, WorkerType};

// mod scoped;
mod signal;
mod utils;

pub(crate) struct ScopeData {
    num_running_threads: AtomicUsize,
    a_thread_panicked: AtomicBool,
    signal: Signal,
}

impl ScopeData {
    pub(crate) fn increment_num_running_threads(&self) {
        // We check for 'overflow' with usize::MAX / 2, to make sure there's no
        // chance it overflows to 0, which would result in unsoundness.
        if self.num_running_threads.fetch_add(1, Ordering::Relaxed) > usize::MAX / 2 {
            // This can only reasonably happen by mem::forget()'ing a lot of ScopedJoinHandles.
            self.decrement_num_running_threads(false);
            panic!("too many running threads in thread scope");
        }
    }

    pub(crate) fn decrement_num_running_threads(&self, panic: bool) {
        if panic {
            self.a_thread_panicked.store(true, Ordering::Relaxed);
        }

        if self.num_running_threads.fetch_sub(1, Ordering::Release) == 1 {
            // All threads have terminated
            self.signal.signal();
        }
    }
}

struct WebWorkerContext {
    // The structure here is a little convoluted in order to give a
    // ‘work-dealing’ API:
    // - first, we have a `Send` function, that we send to the thread
    // - that function may produce a non-`Send` future to run,
    //   since that future will never be moved across threads again
    work: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>,
}

impl WebWorkerContext {
    unsafe fn new<'a, 'scope: 'a, Fut: Future<Output: Send + 'a> + 'a>(work: impl FnOnce() -> Fut + Send + 'a, scope_data: Option<Arc<ScopeData>>) -> (Self, Arc<Signal>, Arc<Packet<'scope, Fut::Output>>) {
        let my_signal = Arc::new(Signal::new());
        let their_signal = my_signal.clone();

        let my_packet: Arc<Packet<'scope, Fut::Output>> = Arc::new(Packet {
            scope: scope_data,
            result: UnsafeCell::new(None),
            _marker: PhantomData,
        });
        let their_packet = my_packet.clone();

        // Pass `f` in `MaybeUninit` because actually that closure might *run longer than the lifetime of `F`*.
        // See <https://github.com/rust-lang/rust/issues/101983> for more details.
        // To prevent leaks we use a wrapper that drops its contents.
        #[repr(transparent)]
        struct MaybeDangling<T>(mem::MaybeUninit<T>);
        impl<T> MaybeDangling<T> {
            fn new(x: T) -> Self {
                MaybeDangling(mem::MaybeUninit::new(x))
            }
            fn into_inner(self) -> T {
                // SAFETY: we are always initiailized.
                let ret = unsafe { self.0.assume_init_read() };
                // Make sure we don't drop.
                mem::forget(self);
                ret
            }
        }
        impl<T> Drop for MaybeDangling<T> {
            fn drop(&mut self) {
                // SAFETY: we are always initiailized.
                unsafe { self.0.assume_init_drop() };
            }
        }

        let work = MaybeDangling::new(work);
        let main = Box::new(move || {
            // SAFETY: we constructed `work` initialized.
            let work = work.into_inner();
            // Execute the closure and catch any panics
            let try_result = std::panic::catch_unwind(AssertUnwindSafe(work));
            Box::pin(async move {
                let try_result = match try_result {
                    Ok(fut) => AssertUnwindSafe(fut).catch_unwind().await,
                    Err(e) => Err(e),
                };

                // SAFETY: `their_packet` as been built just above and moved by the
                // closure (it is an Arc<...>) and `my_packet` will be stored in the
                // same `JoinInner` as this closure meaning the mutation will be
                // safe (not modify it and affect a value far away).
                unsafe { *their_packet.result.get() = Some(try_result) };
                // Here `their_packet` gets dropped, and if this is the last `Arc` for that packet that
                // will call `decrement_num_running_threads` and therefore signal that this thread is
                // done.
                drop(their_packet);
                // Notify waiting handles
                their_signal.signal();
                // Here, the lifetime `'a` and even `'scope` can end. `main` keeps running for a bit
                // after that before returning itself.
            }) as Pin<Box<dyn Future<Output = ()> + 'a>>
        });

        (
            Self {
                // Erase lifetime
                work: mem::transmute::<Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + 'a>> + Send + 'a>, Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>>(main),
            },
            my_signal,
            my_packet,
        )
    }
}

/// Entry point for web workers
#[wasm_bindgen]
pub async fn wasm_thread_entry_point(ptr: u32) {
    let ctx = unsafe { Box::from_raw(ptr as *mut WebWorkerContext) };
    std::pin::pin!((ctx.work)()).await;
    WorkerMessage::ThreadComplete.post();
}

/// Web worker to main thread messages
enum WorkerMessage {
    /// Thread has completed execution
    ThreadComplete,
}

impl WorkerMessage {
    pub fn post(self) {
        let req = Box::new(self);

        js_sys::global()
            .dyn_into::<DedicatedWorkerGlobalScope>()
            .unwrap()
            .post_message(&JsValue::from(Box::into_raw(req) as u32))
            .unwrap();
    }
}

static DEFAULT_BUILDER: Mutex<Option<Builder>> = Mutex::new(None);

/// Thread factory, which can be used in order to configure the properties of a new thread.
#[derive(Debug, Clone)]
pub struct Builder {
    // A name for the thread-to-be, for identification in panic messages
    name: Option<String>,
    // A prefix for the thread-to-be, for identification in panic messages
    prefix: Option<String>,
    // The URL of the web worker script to use as web worker thread script
    worker_script_url: Option<String>,
    // The size of the stack for the spawned thread in bytes
    stack_size: Option<usize>,
    // Url of the `wasm_bindgen` generated shim `.js` script to use as web worker entry point
    wasm_bindgen_shim_url: Option<String>,
}

impl Default for Builder {
    fn default() -> Self {
        DEFAULT_BUILDER.lock_spin().unwrap().clone().unwrap_or(Self::empty())
    }
}

impl Builder {
    /// Creates a builder inheriting global configuration options set by [Self::set_default].
    pub fn new() -> Builder {
        Builder::default()
    }

    /// Creates a builder without inheriting global options set by [Self::set_default].
    pub fn empty() -> Builder {
        Self {
            name: None,
            prefix: None,
            worker_script_url: None,
            stack_size: None,
            wasm_bindgen_shim_url: None,
        }
    }

    /// Sets current values as global default for all new builders created with [Builder::new] or [Builder::default].
    pub fn set_default(self) {
        *DEFAULT_BUILDER.lock_spin().unwrap() = Some(self);
    }

    /// Sets the prefix of the thread.
    pub fn prefix(mut self, prefix: String) -> Builder {
        self.prefix = Some(prefix);
        self
    }

    pub fn worker_script_url(mut self, worker_script_url: String) -> Builder {
        self.worker_script_url = Some(worker_script_url);
        self
    }

    /// Sets the name of the thread.
    ///
    /// If not set, the default name is autogenerated.
    pub fn name(mut self, name: String) -> Builder {
        self.name = Some(name);
        self
    }

    /// Sets the size of the stack (in bytes) for the new thread.
    ///
    /// # Warning
    ///
    /// This is currently not supported by wasm, but provided for API consistency with [std::thread].
    pub fn stack_size(mut self, size: usize) -> Builder {
        self.stack_size = Some(size);
        self
    }

    /// Sets the URL of wasm_bindgen generated shim script.
    pub fn wasm_bindgen_shim_url(mut self, url: String) -> Builder {
        self.wasm_bindgen_shim_url = Some(url);
        self
    }

    /// Spawns a new thread by taking ownership of the `Builder`, and returns an
    /// [std::io::Result] to its [`JoinHandle`].
    pub fn spawn<Fut>(self, f: impl FnOnce() -> Fut + Send + 'static) -> std::io::Result<(Rc<Worker>, JoinHandle<Fut::Output>)>
    where
        Fut: Future<Output: Send + 'static> + 'static,
    {
        unsafe { self.spawn_unchecked(f) }
    }

    /// Spawns a new thread without any lifetime restrictions by taking ownership
    /// of the `Builder`, and returns an [std::io::Result] to its [`JoinHandle`].
    ///
    /// # Safety
    ///
    /// The caller has to ensure that no references in the supplied thread closure
    /// or its return type can outlive the spawned thread's lifetime. This can be
    /// guaranteed in two ways:
    ///
    /// - ensure that [`join`][`JoinHandle::join`] is called before any referenced
    /// data is dropped
    /// - use only types with `'static` lifetime bounds, i.e., those with no or only
    /// `'static` references (both [`Builder::spawn`]
    /// and [`spawn`] enforce this property statically)
    pub unsafe fn spawn_unchecked<'a, Fut>(self, f: impl FnOnce() -> Fut + Send + 'static) -> std::io::Result<(Rc<Worker>, JoinHandle<Fut::Output>)>
    where
        Fut: Future<Output: Send + 'a> + 'a,
    {
        let (worker, join_handle) = unsafe { self.spawn_unchecked_(f, None) }?;
        Ok((worker, JoinHandle(join_handle)))
    }

    pub(crate) unsafe fn spawn_unchecked_<'a, 'scope, Fut>(
        self,
        f: impl FnOnce() -> Fut + Send + 'a,
        scope_data: Option<Arc<ScopeData>>,
    ) -> std::io::Result<(Rc<Worker>, JoinInner<'scope, Fut::Output>)>
    where
        Fut: Future<Output: Send + 'a> + 'a,
        'scope: 'a,
    {
        let (context, my_signal, my_packet) = WebWorkerContext::new(f, scope_data);

        let worker = self.spawn_for_context(context);

        if let Some(scope) = &my_packet.scope {
            scope.increment_num_running_threads();
        }

        Ok((
            worker,
            JoinInner {
                signal: my_signal,
                packet: my_packet,
            },
        ))
    }

    unsafe fn spawn_for_context(self, ctx: WebWorkerContext) -> Rc<Worker> {
        let Builder {
            name,
            prefix,
            worker_script_url,
            wasm_bindgen_shim_url,
            ..
        } = self;

        let wasm_bindgen_shim_url = wasm_bindgen_shim_url.unwrap_or_else(get_wasm_bindgen_shim_script_path);

        // Todo: figure out how to set stack size
        let options = WorkerOptions::new();
        match (name, prefix) {
            (Some(name), Some(prefix)) => options.set_name(&format!("{}:{}", prefix, name)),
            (Some(name), None) => options.set_name(&name),
            (None, Some(prefix)) => {
                let random = (js_sys::Math::random() * 10e10) as u64;
                options.set_name(&format!("{}:{}", prefix, random));
            }
            (None, None) => (),
        };

        options.set_type(WorkerType::Module);

        // Spawn the worker
        let worker = Rc::new(Worker::new_with_options(
            worker_script_url.unwrap_or_else(|| wasm_bindgen::link_to!(module = "/src/wasm32/js/web_worker_module.js")).as_str(),
            &options,
        ).unwrap());

        // Make copy and keep a reference in callback handler so that GC does not despawn worker
        let mut their_worker = Some(worker.clone());

        let callback = Closure::wrap(Box::new(move |x: &web_sys::MessageEvent| {
            // All u32 bits map to f64 mantisa so it's safe to cast like that
            let req = Box::from_raw(x.data().as_f64().unwrap() as u32 as *mut WorkerMessage);

            match *req {
                WorkerMessage::ThreadComplete => {
                    // Drop worker reference so it can be cleaned up by GC
                    their_worker.take();
                }
            };
        }) as Box<dyn FnMut(&web_sys::MessageEvent)>);
        worker.set_onmessage(Some(callback.as_ref().unchecked_ref()));

        // TODO: cleanup this leak somehow
        callback.forget();

        let ctx_ptr = Box::into_raw(Box::new(ctx));

        // Pack shared wasm (module and memory) and work as a single JS array
        let init = js_sys::Array::new();
        init.push(&wasm_bindgen_shim_url.into());
        init.push(&wasm_bindgen::module());
        init.push(&wasm_bindgen::memory());
        init.push(&JsValue::from(ctx_ptr as u32));

        // Send initialization message
        match worker.post_message(&init) {
            Ok(()) => Ok(worker),
            Err(e) => {
                drop(Box::from_raw(ctx_ptr));
                Err(e)
            }
        }
        .unwrap()
    }
}

// This packet is used to communicate the return value between the spawned
// thread and the rest of the program. It is shared through an `Arc` and
// there's no need for a mutex here because synchronization happens with `join()`
// (the caller will never read this packet until the thread has exited).
//
// An Arc to the packet is stored into a `JoinInner` which in turns is placed
// in `JoinHandle`.
struct Packet<'scope, T> {
    scope: Option<Arc<ScopeData>>,
    result: UnsafeCell<Option<Result<T>>>,
    _marker: PhantomData<Option<&'scope ScopeData>>,
}

// Due to the usage of `UnsafeCell` we need to manually implement Sync.
// The type `T` should already always be Send (otherwise the thread could not
// have been created) and the Packet is Sync because all access to the
// `UnsafeCell` synchronized (by the `join()` boundary), and `ScopeData` is Sync.
unsafe impl<'scope, T: Send> Sync for Packet<'scope, T> {}

impl<'scope, T> Drop for Packet<'scope, T> {
    fn drop(&mut self) {
        // If this packet was for a thread that ran in a scope, the thread
        // panicked, and nobody consumed the panic payload, we make sure
        // the scope function will panic.
        let unhandled_panic = matches!(self.result.get_mut(), Some(Err(_)));
        // Drop the result without causing unwinding.
        // This is only relevant for threads that aren't join()ed, as
        // join() will take the `result` and set it to None, such that
        // there is nothing left to drop here.
        // If this panics, we should handle that, because we're outside the
        // outermost `catch_unwind` of our thread.
        // We just abort in that case, since there's nothing else we can do.
        // (And even if we tried to handle it somehow, we'd also need to handle
        // the case where the panic payload we get out of it also panics on
        // drop, and so on. See issue #86027.)
        if let Err(_) = catch_unwind(AssertUnwindSafe(|| {
            *self.result.get_mut() = None;
        })) {
            panic!("thread result panicked on drop");
        }
        // Book-keeping so the scope knows when it's done.
        if let Some(scope) = &self.scope {
            // Now that there will be no more user code running on this thread
            // that can use 'scope, mark the thread as 'finished'.
            // It's important we only do this after the `result` has been dropped,
            // since dropping it might still use things it borrowed from 'scope.
            scope.decrement_num_running_threads(unhandled_panic);
        }
    }
}

/// Inner representation for JoinHandle
pub(crate) struct JoinInner<'scope, T> {
    packet: Arc<Packet<'scope, T>>,
    signal: Arc<Signal>,
}

impl<'scope, T> JoinInner<'scope, T> {
    pub fn join(mut self) -> Result<T> {
        self.signal.wait();
        Arc::get_mut(&mut self.packet).unwrap().result.get_mut().take().unwrap()
    }

    pub async fn join_async(mut self) -> Result<T> {
        self.signal.wait_async().await;
        Arc::get_mut(&mut self.packet).unwrap().result.get_mut().take().unwrap()
    }
}

/// An owned permission to join on a thread (block on its termination).
pub struct JoinHandle<T>(JoinInner<'static, T>);

impl<T> JoinHandle<T> {
    /// Extracts a handle to the underlying thread.
    pub fn thread(&self) -> &Thread {
        unimplemented!();
        //&self.0.thread
    }

    /// Waits for the associated thread to finish.
    pub fn join(self) -> Result<T> {
        self.0.join()
    }

    /// Waits for the associated thread to finish asynchronously.
    pub async fn join_async(self) -> Result<T> {
        self.0.join_async().await
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle").finish_non_exhaustive()
    }
}

/// Spawns a new thread, returning a JoinHandle for it.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    Builder::new().spawn(|| async move { f() }).expect("failed to spawn thread").1
}
