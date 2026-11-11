//! The stack budget the depth-limit tests run on.
//!
//! Those tests protect the promise that a document at the parser's depth limit executes on a
//! default Tokio worker stack (2 MiB). A release build keeps that promise with plenty of room:
//! about 0.5 MiB on x86_64, with or without `tracing`. An unoptimised build does not — it needs
//! roughly 2.0 MiB without `tracing` and 2.1 MiB with it — so a debug run checks a larger budget
//! instead of a bound it misses by a few percent.
//!
//! Set `STACK_KB` to override either budget when bisecting a regression.

/// The stack size for the depth-limit tests, in bytes.
pub fn stack_size() -> usize {
    match std::env::var("STACK_KB")
        .ok()
        .and_then(|kb| kb.parse::<usize>().ok())
    {
        Some(kb) => kb * 1024,
        None if cfg!(debug_assertions) => 4 * 1024 * 1024,
        None => 2 * 1024 * 1024,
    }
}
