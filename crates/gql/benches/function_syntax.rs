//! Resolver representation microbenchmark for issue #2 ("Function syntax").
//!
//! Run with:
//!
//! ```sh
//! cargo bench -p gql --bench function_syntax
//! ```
//!
//! The issue asks whether a field's resolver should stay an `Arc<dyn Fn>` (what
//! [`gql::ResolveFn`] is today) or become one of:
//!
//! 1. `Box<dyn Fn()>`, called through a shared reference;
//! 2. an enum `{ Ptr(fn()), Boxed(Box<dyn Fn()>) }`, where non-capturing resolvers are a bare
//!    function pointer and therefore need no heap allocation;
//! 3. `fn(&dyn Any)` plus a downcast to the boxed closure at call time.
//!
//! Each representation is measured on the three things a schema actually pays for:
//!
//! * **call** — dispatch the resolver, build its boxed future, poll it once to completion and
//!   drop it. This is the per-field cost, and it is what matters at request time.
//! * **build** — construct the resolver slot from a capturing closure, and from a non-capturing
//!   function item. This is paid once per field when a schema is built.
//! * **clone** — clone the slot. `Field` is `Clone`, so every representation has to be too. No
//!   request path clones a `Field`: [`gql::Schema`] is one `Arc<SchemaInner>` and execution
//!   borrows the definition while calling its resolver. The clone column therefore bounds an API
//!   cost, not a per-request one; the clone cost is unreachable from a request path.
//!
//! A second, dispatch-only table repeats the same representations with a plain `u64` return so
//! the (single-digit nanosecond) dispatch difference is not hidden by the boxed-future
//! allocation every representation pays identically. The dispatch-only numbers are not a
//! realistic resolver; they exist to isolate the syntax question.
//!
//! The context passed to the resolvers is a stand-in with the same shape and size as
//! [`gql::ResolverContext`] (four borrowed fields). Using the real one would only add the cost
//! of building an `ExecutionContext`, which is identical for every representation.
//!
//! The futures borrow the context exactly like `gql::ResolveFuture<'a>` does, so the measured
//! call path has the same lifetime plumbing as the real one: `Box::pin` of a resolver future
//! allocates once, and that allocation is common to all representations. It is also the dominant
//! term in the call column, so the call numbers hide most of the indirection the dispatch column
//! exposes.
//!
//! Allocations are counted by a thin wrapper around the system allocator, the same style as
//! `benches/request.rs`. The wrapper only delegates to `System`; there is no custom memory
//! management.
//!
//! # Result: keep `Arc<dyn Fn>`
//!
//! Apple M5 Max, `--release`, measured 2026-11-10 on `oscar/o/full-graphql-spec` at `d0c8840`
//! (this benchmark itself is uncommitted). Each cell is the **minimum of nine rounds** — the
//! harness reports the best round, not a median — from the middle of three consecutive runs.
//! Across those runs the call column moved by at most 0.18 ns within a variant, and dispatch by at
//! most 0.05 ns apart from a 0.22 ns swing on the monomorphised row. Times are nanoseconds per
//! call or per slot; allocation counts are in parentheses.
//! `n/a` means the measurement does not exist for that representation: a `Ptr` arm has no
//! capturing constructor, and a plain `Box` cannot be cloned.
//!
//! The `arc_from_fn` row repeats the `Arc` slot with the non-capturing body, which splits the
//! `Ptr` row's advantage into the resolver body and the representation itself.
//!
//! | representation                              | call | call allocs | build, capturing | build, fn item | clone       | dispatch |
//! |---------------------------------------------|------|-------------|------------------|----------------|-------------|----------|
//! | `Arc<dyn Fn>` (current)                     | 15.47| 1           | 16.17 (1)        | 15.79 (1)      | 3.10 (0)    | 1.55     |
//! | `Arc<dyn Fn>`, fn-item body                 | 15.31| 1           | n/a              | 15.79 (1)      | 3.10 (0)    | 1.53     |
//! | `Box<dyn Fn>`                               | 15.41| 1           | 13.10 (1)        | 0.34 (0)       | not `Clone` | 1.54     |
//! | enum, `Boxed(Box<dyn Fn>)`                  | 15.54| 1           | 13.19 (1)        | 0.22 (0)       | not `Clone` | 1.52     |
//! | enum, `Ptr(fn)`                             | 15.02| 1           | n/a              | 0.22 (0)       | not `Clone` | 0.71     |
//! | enum, `Boxed(Arc<dyn Fn>)` (cloneable)      | 15.65| 1           | 16.40 (1)        | 0.23 (0)       | 3.09 (0)    | 1.55     |
//! | enum, `Ptr(fn)` (cloneable)                 | 15.02| 1           | n/a              | 0.23 (0)       | 0.22 (0)    | 0.71     |
//! | `fn(&dyn Any)` -> `Box<dyn Fn>`             | 15.96| 1           | 28.77 (2)        | 16.81 (1)      | 3.10 (0)    | 2.19     |
//! | `fn(&dyn Any)` -> monomorphised closure     | 15.22| 1           | 16.35 (1)        | 16.22 (1)      | 3.26 (0)    | 1.44     |
//!
//! What the numbers say:
//!
//! * At request time every representation allocates exactly one boxed future and costs
//!   15.0-16.0 ns per field. The `Ptr` arm is the fastest, but against the like-for-like
//!   `arc_from_fn` row the representation is worth only ~0.2-0.3 ns (~2%); the rest of its
//!   advantage over the capturing `Arc` row comes from the cheaper resolver body, not the slot.
//!   The ~13 ns `Box::pin` every representation pays dominates the call, so the difference is
//!   inside its run-to-run noise and nowhere near the 10% threshold.
//! * The `request` benchmark's 55-field query completes in ~5.1-5.4 µs end to end; execution
//!   itself is ~3.3 µs, about 61 ns per completed field. Only 3 of the 55 completions run a
//!   resolver (`users`, its alias, and `health`); the other 52 take the property path and cannot
//!   see this change. Against a resolver-backed field's ~15.5 ns call, the dispatch-only 0.83 ns
//!   is ~5% — and ~1.4% of the average completed field — not "well under 1%" of a completed
//!   field. Across the whole execution it is at most `3 × 0.83 ≈ 2.5 ns` of ~3.3 µs, or ~0.08%.
//! * Only the dispatch-only rows show a large *relative* win for the function pointer: ~0.72 ns
//!   versus ~1.52 ns, about half of the dispatch. Giving the `Arc` the same non-capturing body
//!   dispatches in the same ~1.52 ns, so the gap is the indirection, not the body. In absolute
//!   terms that is 0.8 ns per dispatch-only call, which is the upper bound on what the enum could
//!   save; the boxed future hides most of it, leaving ~0.2-0.3 ns in the call column.
//!   Benchmarking the syntax instead of the work makes it look like a 2x win; against a realistic
//!   resolver it is low single-digit percent.
//! * The function pointer's real advantage is build time: `Box<dyn Fn>` and the `Ptr` arm
//!   construct a non-capturing resolver with zero allocations, while `Arc<dyn Fn>` always
//!   allocates — an `ArcInner` is not zero-sized even when the closure is — so it pays one
//!   allocation and ~16 ns per field. Schemas are built once, so this is irrelevant to request
//!   latency.
//! * `Box<dyn Fn>` and the plain enum's `Boxed` arm are not `Clone`, but `Field` is. A real
//!   `Field` therefore has to keep an `Arc` in the boxed arm, at which point the captured-closure
//!   path is exactly today's representation plus a match, and the only added value is the `Ptr`
//!   arm. `Field` cloning is not on a request path (`Schema` is `Arc<SchemaInner>`; execution
//!   borrows the definition), so the 3 ns clone cost is unreachable from a request.
//! * The `dyn Any` downcast taken literally is strictly worse: +0.3-0.4 ns per call, two
//!   allocations per field instead of one, and a ~45% slower dispatch. A monomorphised trampoline
//!   only reaches parity with `Arc<dyn Fn>` (and its dispatch is within noise of it), so it buys
//!   nothing.
//!
//! Recommendation: **keep `ResolveFn = Arc<dyn Fn(..) -> ..>`; issue #2 can be closed as
//! resolved.** A `Ptr` arm would only help resolvers that are bare function items, and only if
//! users registered them through a second constructor: an ordinary closure — capturing or not —
//! cannot coerce to `fn`, so `resolve` would have to stay the closure constructor and a new
//! `resolve_fn` would have to exist alongside it. gurafi's builders install closures that capture
//! `Arc`s (`expose`, for example, clones the `Arc` getter into each resolver), so those take the
//! `Arc` arm regardless. Adding the enum would change the public type of `Field::resolve`, add a
//! match to every field completion, and save ~0.2-0.3 ns on a resolver-backed field — inside
//! noise, and more than an order of magnitude inside the 10% threshold.
//!
//! The conclusion depends on `ResolveFuture<'a>` remaining a boxed future. The mandatory
//! `Box::pin` (~13 ns) is what keeps every representation within noise of every other; if that
//! boxing ever went away, dispatch would be a visible share of a resolver-backed field and this
//! trade-off would need re-measuring. The worthwhile target is that allocation and a gurafi fast
//! path for non-resolver properties, not the resolver indirection.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    any::Any,
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use futures::future::BoxFuture;
use gql::{Object, Value};

// -------------------------------------------------------------------------------------------
// Allocation counting
// -------------------------------------------------------------------------------------------

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// -------------------------------------------------------------------------------------------
// A `ResolverContext`-shaped argument
// -------------------------------------------------------------------------------------------

/// The per-request state a resolver can reach. Only its address is used here.
struct RequestState {
    #[allow(dead_code)]
    variables: u32,
}

/// The static/dynamic field information, mirroring `gql::ResolveInfo`'s borrowed fields.
struct Info<'a> {
    field_name: &'a str,
    parent_type: &'a str,
    #[allow(dead_code)]
    path: &'a [&'a str],
}

/// Same four borrowed fields, and therefore the same 32-byte size, as `gql::ResolverContext`.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    #[allow(dead_code)]
    parent: &'a Value,
    #[allow(dead_code)]
    args: &'a Object,
    #[allow(dead_code)]
    context: &'a RequestState,
    info: &'a Info<'a>,
}

impl Ctx<'_> {
    /// Touch the context synchronously, the way a resolver reads its field name and arguments
    /// before awaiting anything.
    fn field_len(&self) -> i32 {
        self.info.field_name.len() as i32 + self.info.parent_type.len() as i32
    }
}

type Fut<'a> = BoxFuture<'a, Value>;

/// A resolver as a plain function pointer: no heap allocation when it captures nothing.
type ResolverPtr = for<'a> fn(Ctx<'a>) -> Fut<'a>;

/// A resolver as a boxed/arc'd closure object, the shape `gql::ResolveFn` has today.
type ResolverDyn = dyn for<'a> Fn(Ctx<'a>) -> Fut<'a> + Send + Sync;

// -------------------------------------------------------------------------------------------
// The representations
// -------------------------------------------------------------------------------------------

/// Today's `gql::ResolveFn`: one `Arc` allocation, clones are a refcount bump.
#[derive(Clone)]
struct ArcResolver(Arc<ResolverDyn>);

impl ArcResolver {
    fn from_capturing() -> Self {
        Self(Arc::new(capturing()))
    }

    fn from_function() -> Self {
        Self(Arc::new(non_capturing))
    }

    fn call<'a>(&self, ctx: Ctx<'a>) -> Fut<'a> {
        (self.0)(ctx)
    }
}

/// The issue's first suggestion: a boxed trait object, called through a shared reference.
///
/// Not `Clone`, so a `Field` holding one would have to store it behind an `Arc` (which brings
/// back today's representation plus an extra allocation at build time).
struct BoxResolver(Box<ResolverDyn>);

impl BoxResolver {
    fn from_capturing() -> Self {
        Self(Box::new(capturing()))
    }

    fn from_function() -> Self {
        Self(Box::new(non_capturing))
    }

    fn call<'a>(&self, ctx: Ctx<'a>) -> Fut<'a> {
        (self.0)(ctx)
    }
}

/// The issue's second suggestion: a function pointer for non-capturing resolvers, a box for
/// capturing ones. Function items are zero-sized, so the `Ptr` arm allocates nothing.
enum EnumResolver {
    Ptr(ResolverPtr),
    Boxed(Box<ResolverDyn>),
}

impl EnumResolver {
    fn from_capturing() -> Self {
        Self::Boxed(Box::new(capturing()))
    }

    fn from_function() -> Self {
        Self::Ptr(non_capturing)
    }

    fn call<'a>(&self, ctx: Ctx<'a>) -> Fut<'a> {
        match self {
            Self::Ptr(function) => function(ctx),
            Self::Boxed(function) => function(ctx),
        }
    }
}

/// The same enum as it would have to exist inside `Field`, which is `Clone`: the boxed arm holds
/// an `Arc` so the enum stays cloneable. `Ptr` clones for free.
#[derive(Clone)]
enum CloneableEnumResolver {
    Ptr(ResolverPtr),
    Boxed(Arc<ResolverDyn>),
}

impl CloneableEnumResolver {
    fn from_capturing() -> Self {
        Self::Boxed(Arc::new(capturing()))
    }

    fn from_function() -> Self {
        Self::Ptr(non_capturing)
    }

    fn call<'a>(&self, ctx: Ctx<'a>) -> Fut<'a> {
        match self {
            Self::Ptr(function) => function(ctx),
            Self::Boxed(function) => function(ctx),
        }
    }
}

/// The issue's third suggestion, taken literally: the slot erases its state to `dyn Any` and the
/// call site downcasts it back to `Box<dyn Fn>` before dispatching.
///
/// Note the two allocations at build time: one for the boxed closure, one for the `Arc` that
/// erases it. Cloning is a refcount bump.
#[derive(Clone)]
struct AnyResolver {
    call: for<'a> fn(&'a (dyn Any + Send + Sync), Ctx<'a>) -> Fut<'a>,
    state: Arc<dyn Any + Send + Sync>,
}

impl AnyResolver {
    fn from_capturing() -> Self {
        Self::from_boxed(Box::new(capturing()))
    }

    fn from_function() -> Self {
        Self::from_boxed(Box::new(non_capturing))
    }

    fn from_boxed(function: Box<ResolverDyn>) -> Self {
        fn call<'a>(state: &'a (dyn Any + Send + Sync), ctx: Ctx<'a>) -> Fut<'a> {
            let function = state
                .downcast_ref::<Box<ResolverDyn>>()
                .expect("resolver state holds a boxed closure");
            function(ctx)
        }
        Self {
            call,
            state: Arc::new(function),
        }
    }

    fn call<'a>(&'a self, ctx: Ctx<'a>) -> Fut<'a> {
        (self.call)(&*self.state, ctx)
    }
}

/// The `dyn Any` idea done the way a real framework would: a monomorphised trampoline downcasts
/// the state to the concrete closure type `F` and calls it without a virtual dispatch.
///
/// Build-time cost is one `Arc` allocation (none for a zero-sized function item), exactly like
/// today's representation, so this is the strongest version of suggestion three.
#[derive(Clone)]
struct TrampolineResolver {
    call: for<'a> fn(&'a (dyn Any + Send + Sync), Ctx<'a>) -> Fut<'a>,
    state: Arc<dyn Any + Send + Sync>,
}

fn trampoline<'a, F>(state: &'a (dyn Any + Send + Sync), ctx: Ctx<'a>) -> Fut<'a>
where
    F: for<'x> Fn(Ctx<'x>) -> Fut<'x> + Send + Sync + 'static,
{
    let function = state.downcast_ref::<F>().expect("resolver state type");
    function(ctx)
}

impl TrampolineResolver {
    fn from_capturing() -> Self {
        Self::from_function(capturing())
    }

    fn from_function<F>(function: F) -> Self
    where
        F: for<'x> Fn(Ctx<'x>) -> Fut<'x> + Send + Sync + 'static,
    {
        Self {
            call: trampoline::<F>,
            state: Arc::new(function),
        }
    }

    fn call<'a>(&'a self, ctx: Ctx<'a>) -> Fut<'a> {
        (self.call)(&*self.state, ctx)
    }
}

// -------------------------------------------------------------------------------------------
// Sample resolvers
// -------------------------------------------------------------------------------------------

/// A capturing resolver. Real resolvers capture their data source (an `Arc` to a pool, a service,
/// a model); a copied `i32` stands in for that here so the measurement is about the slot, not
/// the captured state.
fn capturing() -> impl for<'x> Fn(Ctx<'x>) -> Fut<'x> + Send + Sync + 'static {
    let captured = 1i32;
    move |ctx: Ctx<'_>| -> Fut<'_> {
        Box::pin(async move { Value::Int(captured + ctx.field_len()) })
    }
}

/// A non-capturing resolver: a plain function item, which coerces to both `ResolverPtr` and
/// `&dyn Fn` and is zero-sized.
fn non_capturing(ctx: Ctx<'_>) -> Fut<'_> {
    Box::pin(async move { Value::Int(1 + ctx.field_len()) })
}

// -------------------------------------------------------------------------------------------
// Dispatch-only family (no boxed future) to isolate the syntax cost
// -------------------------------------------------------------------------------------------

mod dispatch {
    use super::{Ctx, black_box};

    pub type Ptr = for<'a> fn(Ctx<'a>) -> u64;
    pub type Dyn = dyn for<'a> Fn(Ctx<'a>) -> u64 + Send + Sync;

    pub fn capturing() -> impl for<'x> Fn(Ctx<'x>) -> u64 + Send + Sync + 'static {
        let captured = 1u64;
        move |ctx: Ctx<'_>| {
            black_box(ctx.field_len());
            captured
        }
    }

    pub fn non_capturing(ctx: Ctx<'_>) -> u64 {
        black_box(ctx.field_len());
        1
    }

    pub struct ArcDyn(pub std::sync::Arc<Dyn>);

    impl ArcDyn {
        pub fn call(&self, ctx: Ctx<'_>) -> u64 {
            (self.0)(ctx)
        }
    }

    pub struct BoxDyn(pub Box<Dyn>);

    impl BoxDyn {
        pub fn call(&self, ctx: Ctx<'_>) -> u64 {
            (self.0)(ctx)
        }
    }

    pub enum Enum {
        Ptr(Ptr),
        Boxed(Box<Dyn>),
    }

    impl Enum {
        pub fn call(&self, ctx: Ctx<'_>) -> u64 {
            match self {
                Self::Ptr(function) => function(ctx),
                Self::Boxed(function) => function(ctx),
            }
        }
    }

    #[derive(Clone)]
    pub enum CloneableEnum {
        Ptr(Ptr),
        Boxed(std::sync::Arc<Dyn>),
    }

    impl CloneableEnum {
        pub fn call(&self, ctx: Ctx<'_>) -> u64 {
            match self {
                Self::Ptr(function) => function(ctx),
                Self::Boxed(function) => function(ctx),
            }
        }
    }

    pub struct AnyDowncast {
        pub call: for<'a> fn(&'a (dyn std::any::Any + Send + Sync), Ctx<'a>) -> u64,
        pub state: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    }

    pub fn any_call<'a>(state: &'a (dyn std::any::Any + Send + Sync), ctx: Ctx<'a>) -> u64 {
        let function = state
            .downcast_ref::<Box<Dyn>>()
            .expect("resolver state holds a boxed closure");
        function(ctx)
    }

    impl AnyDowncast {
        pub fn from_boxed(function: Box<Dyn>) -> Self {
            Self {
                call: any_call,
                state: std::sync::Arc::new(function),
            }
        }

        pub fn call(&self, ctx: Ctx<'_>) -> u64 {
            (self.call)(&*self.state, ctx)
        }
    }

    pub struct Trampoline {
        pub call: for<'a> fn(&'a (dyn std::any::Any + Send + Sync), Ctx<'a>) -> u64,
        pub state: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    }

    pub fn trampoline<'a, F>(state: &'a (dyn std::any::Any + Send + Sync), ctx: Ctx<'a>) -> u64
    where
        F: for<'x> Fn(Ctx<'x>) -> u64 + Send + Sync + 'static,
    {
        let function = state.downcast_ref::<F>().expect("resolver state type");
        function(ctx)
    }

    impl Trampoline {
        pub fn from_function<F>(function: F) -> Self
        where
            F: for<'x> Fn(Ctx<'x>) -> u64 + Send + Sync + 'static,
        {
            Self {
                call: trampoline::<F>,
                state: std::sync::Arc::new(function),
            }
        }

        pub fn call(&self, ctx: Ctx<'_>) -> u64 {
            (self.call)(&*self.state, ctx)
        }
    }
}

// -------------------------------------------------------------------------------------------
// Measurement
// -------------------------------------------------------------------------------------------

/// Calls measured per variant. Batched so the loop and `black_box` overhead is amortised and
/// per-call differences well under a nanosecond are resolvable.
const ITERATIONS: usize = 2_000_000;
const CALL_BATCH: usize = 16;
const DISPATCH_ITERATIONS: usize = 4_000_000;
const DISPATCH_BATCH: usize = 64;
const ROUNDS: usize = 9;

/// Best-of-`ROUNDS` wall time per iteration plus allocations per iteration.
fn measure<F, R>(iterations: usize, mut operation: F) -> (f64, usize)
where
    F: FnMut() -> R,
{
    for _ in 0..1_000 {
        operation();
    }

    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..ROUNDS {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        let elapsed = started.elapsed();
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        best = best.min(elapsed);
    }
    (
        best.as_secs_f64() * 1e9 / iterations as f64,
        allocations / iterations,
    )
}

/// Measure `calls` calls, made `batch` at a time, as nanoseconds and allocations per call.
fn measure_calls<F, R>(calls: usize, batch: usize, mut operation: F) -> (f64, usize)
where
    F: FnMut() -> R,
{
    for _ in 0..batch * 64 {
        operation();
    }

    let iterations = calls / batch;
    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..ROUNDS {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        for _ in 0..iterations {
            for _ in 0..batch {
                operation();
            }
        }
        let elapsed = started.elapsed();
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        best = best.min(elapsed);
    }
    (best.as_secs_f64() * 1e9 / calls as f64, allocations / calls)
}

/// Call the resolver and drive its future to completion once, like the executor does.
fn drive(future: Fut<'_>) {
    let waker = futures::task::noop_waker();
    let mut task = TaskContext::from_waker(&waker);
    let mut future = future;
    match future.as_mut().poll(&mut task) {
        Poll::Ready(value) => {
            black_box(value);
        }
        Poll::Pending => panic!("benchmark resolvers are ready immediately"),
    }
}

struct Row {
    name: &'static str,
    call_ns: f64,
    call_allocs: usize,
    build_capture_ns: Option<f64>,
    build_capture_allocs: Option<usize>,
    build_function_ns: f64,
    build_function_allocs: usize,
    clone_ns: Option<f64>,
    clone_allocs: Option<usize>,
    dispatch_ns: f64,
}

fn main() {
    let parent = Value::Int(1);
    let args = Object::new();
    let state = RequestState { variables: 0 };
    let info = Info {
        field_name: "hello",
        parent_type: "Query",
        path: &[],
    };
    let ctx = Ctx {
        parent: &parent,
        args: &args,
        context: &state,
        info: &info,
    };

    // Every resolver is passed through `black_box` before it is called: otherwise the optimizer
    // can see the concrete closure or function item and devirtualise the call, which would
    // measure a direct call for every representation and hide the very difference under test.

    // --- calls with a boxed future (the realistic per-field cost) ---

    let arc_resolver = black_box(ArcResolver::from_capturing());
    let (arc_call_ns, arc_call_allocs) = measure_calls(ITERATIONS, CALL_BATCH, || {
        let future = arc_resolver.call(black_box(ctx));
        drive(future);
    });

    // The same `Arc` slot around the non-capturing body. Comparing this with `enum_ptr_function`
    // isolates the representation; comparing it with `arc_dyn_capture` isolates the body.
    let arc_fn_resolver = black_box(ArcResolver::from_function());
    let (arc_fn_call_ns, arc_fn_call_allocs) = measure_calls(ITERATIONS, CALL_BATCH, || {
        let future = arc_fn_resolver.call(black_box(ctx));
        drive(future);
    });

    let box_resolver = black_box(BoxResolver::from_capturing());
    let (box_call_ns, box_call_allocs) = measure_calls(ITERATIONS, CALL_BATCH, || {
        let future = box_resolver.call(black_box(ctx));
        drive(future);
    });

    let enum_resolver = black_box(EnumResolver::from_capturing());
    let (enum_call_ns, enum_call_allocs) = measure_calls(ITERATIONS, CALL_BATCH, || {
        let future = enum_resolver.call(black_box(ctx));
        drive(future);
    });

    let enum_ptr_resolver = black_box(EnumResolver::from_function());
    let (enum_ptr_call_ns, enum_ptr_call_allocs) = measure_calls(ITERATIONS, CALL_BATCH, || {
        let future = enum_ptr_resolver.call(black_box(ctx));
        drive(future);
    });

    let cloneable_enum = black_box(CloneableEnumResolver::from_capturing());
    let (cloneable_enum_call_ns, cloneable_enum_call_allocs) =
        measure_calls(ITERATIONS, CALL_BATCH, || {
            let future = cloneable_enum.call(black_box(ctx));
            drive(future);
        });

    let any_resolver = black_box(AnyResolver::from_capturing());
    let (any_call_ns, any_call_allocs) = measure_calls(ITERATIONS, CALL_BATCH, || {
        let future = any_resolver.call(black_box(ctx));
        drive(future);
    });

    let trampoline_resolver = black_box(TrampolineResolver::from_capturing());
    let (trampoline_call_ns, trampoline_call_allocs) =
        measure_calls(ITERATIONS, CALL_BATCH, || {
            let future = trampoline_resolver.call(black_box(ctx));
            drive(future);
        });

    // --- construction ---

    let (arc_build_capture_ns, arc_build_capture_allocs) =
        measure(ITERATIONS, || black_box(ArcResolver::from_capturing()));
    let (arc_build_function_ns, arc_build_function_allocs) =
        measure(ITERATIONS, || black_box(ArcResolver::from_function()));

    let (box_build_capture_ns, box_build_capture_allocs) =
        measure(ITERATIONS, || black_box(BoxResolver::from_capturing()));
    let (box_build_function_ns, box_build_function_allocs) =
        measure(ITERATIONS, || black_box(BoxResolver::from_function()));

    let (enum_build_capture_ns, enum_build_capture_allocs) =
        measure(ITERATIONS, || black_box(EnumResolver::from_capturing()));
    let (enum_build_function_ns, enum_build_function_allocs) =
        measure(ITERATIONS, || black_box(EnumResolver::from_function()));

    let (cloneable_build_capture_ns, cloneable_build_capture_allocs) = measure(ITERATIONS, || {
        black_box(CloneableEnumResolver::from_capturing())
    });
    let (cloneable_build_function_ns, cloneable_build_function_allocs) =
        measure(ITERATIONS, || {
            black_box(CloneableEnumResolver::from_function())
        });

    let (any_build_capture_ns, any_build_capture_allocs) =
        measure(ITERATIONS, || black_box(AnyResolver::from_capturing()));
    let (any_build_function_ns, any_build_function_allocs) =
        measure(ITERATIONS, || black_box(AnyResolver::from_function()));

    let (trampoline_build_capture_ns, trampoline_build_capture_allocs) =
        measure(ITERATIONS, || {
            black_box(TrampolineResolver::from_capturing())
        });
    let (trampoline_build_function_ns, trampoline_build_function_allocs) =
        measure(ITERATIONS, || {
            black_box(TrampolineResolver::from_function(non_capturing))
        });

    // --- cloning ---
    // `BoxResolver` is deliberately absent: a `Box<dyn Fn>` cannot be cloned, so storing it in a
    // `Clone` field definition requires an `Arc`, i.e. today's representation plus an allocation.

    let (arc_clone_ns, arc_clone_allocs) = measure(ITERATIONS, || black_box(arc_resolver.clone()));
    let (cloneable_clone_ns, cloneable_clone_allocs) =
        measure(ITERATIONS, || black_box(cloneable_enum.clone()));
    let cloneable_ptr = black_box(CloneableEnumResolver::from_function());
    let (cloneable_ptr_clone_ns, cloneable_ptr_clone_allocs) =
        measure(ITERATIONS, || black_box(cloneable_ptr.clone()));
    let (any_clone_ns, any_clone_allocs) = measure(ITERATIONS, || black_box(any_resolver.clone()));
    let (trampoline_clone_ns, trampoline_clone_allocs) =
        measure(ITERATIONS, || black_box(trampoline_resolver.clone()));

    // --- dispatch only ---

    let dispatch_arc = black_box(dispatch::ArcDyn(Arc::new(dispatch::capturing())));
    let (dispatch_arc_ns, dispatch_arc_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_arc.call(black_box(ctx)))
        });

    // `Arc` around the non-capturing body, so the dispatch comparison with `Enum::Ptr` holds the
    // resolver body constant.
    let dispatch_arc_fn = black_box(dispatch::ArcDyn(Arc::new(dispatch::non_capturing)));
    let (dispatch_arc_fn_ns, dispatch_arc_fn_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_arc_fn.call(black_box(ctx)))
        });

    let dispatch_box = black_box(dispatch::BoxDyn(Box::new(dispatch::capturing())));
    let (dispatch_box_ns, dispatch_box_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_box.call(black_box(ctx)))
        });

    let dispatch_enum = black_box(dispatch::Enum::Boxed(Box::new(dispatch::capturing())));
    let (dispatch_enum_ns, dispatch_enum_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_enum.call(black_box(ctx)))
        });

    let dispatch_enum_ptr = black_box(dispatch::Enum::Ptr(dispatch::non_capturing));
    let (dispatch_enum_ptr_ns, dispatch_enum_ptr_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_enum_ptr.call(black_box(ctx)))
        });

    let dispatch_cloneable_enum = black_box(dispatch::CloneableEnum::Boxed(Arc::new(
        dispatch::capturing(),
    )));
    let (dispatch_cloneable_enum_ns, dispatch_cloneable_enum_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_cloneable_enum.call(black_box(ctx)))
        });

    let dispatch_cloneable_enum_ptr =
        black_box(dispatch::CloneableEnum::Ptr(dispatch::non_capturing));
    let (dispatch_cloneable_enum_ptr_ns, dispatch_cloneable_enum_ptr_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_cloneable_enum_ptr.call(black_box(ctx)))
        });

    let dispatch_any = black_box(dispatch::AnyDowncast::from_boxed(Box::new(
        dispatch::capturing(),
    )));
    let (dispatch_any_ns, dispatch_any_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_any.call(black_box(ctx)))
        });

    let dispatch_trampoline = black_box(dispatch::Trampoline::from_function(dispatch::capturing()));
    let (dispatch_trampoline_ns, dispatch_trampoline_allocs) =
        measure_calls(DISPATCH_ITERATIONS, DISPATCH_BATCH, || {
            black_box(dispatch_trampoline.call(black_box(ctx)))
        });

    // --- report ---

    println!(
        "BENCH function_syntax iterations={ITERATIONS} dispatch_iterations={DISPATCH_ITERATIONS} rounds={ROUNDS}"
    );

    let rows = [
        Row {
            name: "arc_dyn_capture",
            call_ns: arc_call_ns,
            call_allocs: arc_call_allocs,
            build_capture_ns: Some(arc_build_capture_ns),
            build_capture_allocs: Some(arc_build_capture_allocs),
            build_function_ns: arc_build_function_ns,
            build_function_allocs: arc_build_function_allocs,
            clone_ns: Some(arc_clone_ns),
            clone_allocs: Some(arc_clone_allocs),
            dispatch_ns: dispatch_arc_ns,
        },
        Row {
            name: "arc_from_fn",
            call_ns: arc_fn_call_ns,
            call_allocs: arc_fn_call_allocs,
            // This row's resolver body does not capture, so there is no capturing build.
            build_capture_ns: None,
            build_capture_allocs: None,
            build_function_ns: arc_build_function_ns,
            build_function_allocs: arc_build_function_allocs,
            clone_ns: Some(arc_clone_ns),
            clone_allocs: Some(arc_clone_allocs),
            dispatch_ns: dispatch_arc_fn_ns,
        },
        Row {
            name: "box_dyn_capture",
            call_ns: box_call_ns,
            call_allocs: box_call_allocs,
            build_capture_ns: Some(box_build_capture_ns),
            build_capture_allocs: Some(box_build_capture_allocs),
            build_function_ns: box_build_function_ns,
            build_function_allocs: box_build_function_allocs,
            // Not cloneable: see the note above the clone section.
            clone_ns: None,
            clone_allocs: None,
            dispatch_ns: dispatch_box_ns,
        },
        Row {
            name: "enum_boxed_capture",
            call_ns: enum_call_ns,
            call_allocs: enum_call_allocs,
            build_capture_ns: Some(enum_build_capture_ns),
            build_capture_allocs: Some(enum_build_capture_allocs),
            build_function_ns: enum_build_function_ns,
            build_function_allocs: enum_build_function_allocs,
            clone_ns: None,
            clone_allocs: None,
            dispatch_ns: dispatch_enum_ns,
        },
        Row {
            name: "enum_ptr_function",
            call_ns: enum_ptr_call_ns,
            call_allocs: enum_ptr_call_allocs,
            // A `Ptr` arm has no capturing constructor: `n/a`, not the `Boxed` build.
            build_capture_ns: None,
            build_capture_allocs: None,
            build_function_ns: enum_build_function_ns,
            build_function_allocs: enum_build_function_allocs,
            clone_ns: None,
            clone_allocs: None,
            dispatch_ns: dispatch_enum_ptr_ns,
        },
        Row {
            name: "enum_cloneable_boxed",
            call_ns: cloneable_enum_call_ns,
            call_allocs: cloneable_enum_call_allocs,
            build_capture_ns: Some(cloneable_build_capture_ns),
            build_capture_allocs: Some(cloneable_build_capture_allocs),
            build_function_ns: cloneable_build_function_ns,
            build_function_allocs: cloneable_build_function_allocs,
            clone_ns: Some(cloneable_clone_ns),
            clone_allocs: Some(cloneable_clone_allocs),
            dispatch_ns: dispatch_cloneable_enum_ns,
        },
        Row {
            name: "enum_cloneable_ptr",
            call_ns: enum_ptr_call_ns,
            call_allocs: enum_ptr_call_allocs,
            build_capture_ns: None,
            build_capture_allocs: None,
            build_function_ns: cloneable_build_function_ns,
            build_function_allocs: cloneable_build_function_allocs,
            clone_ns: Some(cloneable_ptr_clone_ns),
            clone_allocs: Some(cloneable_ptr_clone_allocs),
            dispatch_ns: dispatch_cloneable_enum_ptr_ns,
        },
        Row {
            name: "any_downcast",
            call_ns: any_call_ns,
            call_allocs: any_call_allocs,
            build_capture_ns: Some(any_build_capture_ns),
            build_capture_allocs: Some(any_build_capture_allocs),
            build_function_ns: any_build_function_ns,
            build_function_allocs: any_build_function_allocs,
            clone_ns: Some(any_clone_ns),
            clone_allocs: Some(any_clone_allocs),
            dispatch_ns: dispatch_any_ns,
        },
        Row {
            name: "any_trampoline",
            call_ns: trampoline_call_ns,
            call_allocs: trampoline_call_allocs,
            build_capture_ns: Some(trampoline_build_capture_ns),
            build_capture_allocs: Some(trampoline_build_capture_allocs),
            build_function_ns: trampoline_build_function_ns,
            build_function_allocs: trampoline_build_function_allocs,
            clone_ns: Some(trampoline_clone_ns),
            clone_allocs: Some(trampoline_clone_allocs),
            dispatch_ns: dispatch_trampoline_ns,
        },
    ];

    println!(
        "{:<22} {:>9} {:>7} {:>12} {:>7} {:>12} {:>7} {:>8} {:>7} {:>11}",
        "variant",
        "call_ns",
        "allocs",
        "build_cap_ns",
        "allocs",
        "build_fn_ns",
        "allocs",
        "clone_ns",
        "allocs",
        "dispatch_ns",
    );
    for row in &rows {
        println!(
            "{:<22} {:>9.2} {:>7} {:>12} {:>7} {:>12.2} {:>7} {:>8} {:>7} {:>11.2}",
            row.name,
            row.call_ns,
            row.call_allocs,
            row.build_capture_ns
                .map_or_else(|| "-".to_string(), |ns| format!("{ns:.2}")),
            row.build_capture_allocs
                .map_or_else(|| "-".to_string(), |allocs| allocs.to_string()),
            row.build_function_ns,
            row.build_function_allocs,
            row.clone_ns
                .map_or_else(|| "-".to_string(), |ns| format!("{ns:.2}")),
            row.clone_allocs
                .map_or_else(|| "-".to_string(), |allocs| allocs.to_string()),
            row.dispatch_ns,
        );
    }

    println!(
        "ALLOC dispatch_arc={dispatch_arc_allocs} dispatch_arc_fn={dispatch_arc_fn_allocs} dispatch_box={dispatch_box_allocs} dispatch_enum={dispatch_enum_allocs} dispatch_enum_ptr={dispatch_enum_ptr_allocs} dispatch_enum_cloneable={dispatch_cloneable_enum_allocs} dispatch_enum_cloneable_ptr={dispatch_cloneable_enum_ptr_allocs} dispatch_any={dispatch_any_allocs} dispatch_trampoline={dispatch_trampoline_allocs}"
    );
}
