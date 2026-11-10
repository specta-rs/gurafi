//! Timing probes for the `gurafi` runtime hot paths.
//!
//! These are `#[ignore]`d so a normal `cargo test` run does not spend time on them:
//!
//! ```sh
//! cargo test -p gurafi --release --test perf_probe -- --ignored --nocapture --test-threads=1
//! ```

use std::time::{Duration, Instant};

use futures::executor::block_on;
use gurafi::{Loader, Object, RequestCache, Schema, expose};

fn report(label: &str, iterations: usize, elapsed: Duration, per: usize) {
    let nanos = elapsed.as_nanos() as f64 / iterations as f64;
    let per_item = elapsed.as_nanos() as f64 / (iterations * per.max(1)) as f64;
    println!(
        "{label}: {iterations} iters in {elapsed:?} => {nanos:.0} ns/iter, {per_item:.1} ns/item"
    );
}

fn time<F: FnMut()>(iterations: usize, mut f: F) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    start.elapsed()
}

fn loader() -> Loader<u64, u64> {
    Loader::new(|keys: Vec<u64>| async move { keys.into_iter().map(Ok).collect() })
}

#[test]
#[ignore = "timing probe"]
fn probe_loader_load_many() {
    const KEYS: u64 = 1_000;
    let iterations = 2_000;

    for _ in 0..20 {
        let loader = loader();
        block_on(loader.load_many(0..KEYS));
    }

    let elapsed = time(iterations, || {
        let loader = loader();
        let results = block_on(loader.load_many(0..KEYS));
        assert_eq!(results.len() as u64, KEYS);
    });
    report(
        "Loader::load_many(1000)",
        iterations,
        elapsed,
        KEYS as usize,
    );
}

#[test]
#[ignore = "timing probe"]
fn probe_loader_single_loads() {
    const KEYS: u64 = 500;
    let iterations = 2_000;

    for _ in 0..20 {
        let loader = loader();
        block_on(async {
            for key in 0..KEYS {
                let _ = loader.load(key).await;
            }
        });
    }

    let elapsed = time(iterations, || {
        let loader = loader();
        block_on(async {
            for key in 0..KEYS {
                let _ = loader.load(key).await;
            }
        });
    });
    report("Loader::load x500", iterations, elapsed, KEYS as usize);
}

#[test]
#[ignore = "timing probe"]
fn probe_loader_cached_loads() {
    const KEYS: u64 = 500;
    let iterations = 2_000;
    let loader = loader();
    block_on(loader.load_many(0..KEYS));

    let elapsed = time(iterations, || {
        block_on(async {
            for key in 0..KEYS {
                let _ = loader.load(key).await;
            }
        });
    });
    report(
        "Loader::load cached x500",
        iterations,
        elapsed,
        KEYS as usize,
    );
}

#[test]
#[ignore = "timing probe"]
fn probe_request_cache() {
    let cache = RequestCache::new();
    let _: std::sync::Arc<String> = cache.get_or_insert_with(|| String::from("value"));
    let iterations = 200_000;

    let elapsed = time(iterations, || {
        let value = cache.get_or_insert_with(|| String::from("unused"));
        std::hint::black_box(&value);
    });
    report("RequestCache::get_or_insert_with", iterations, elapsed, 1);
}

#[derive(Clone, Object)]
struct Item {
    name: String,
}

fn register_objects(types: usize) -> Schema<()> {
    let mut schema = Schema::<()>::new();
    for index in 0..types {
        let name = format!("Item{index}");
        let reference = schema.object_ref::<Item>(name.as_str());
        schema.implement(&reference, |object| {
            object.expose(expose!(Item::name));
        });
    }
    schema
}

#[test]
#[ignore = "timing probe"]
fn probe_register_fields() {
    const FIELDS: usize = 500;
    let names: Vec<&'static str> = (0..FIELDS)
        .map(|index| Box::leak(format!("field{index}").into_boxed_str()) as &'static str)
        .collect();
    let iterations = 200;

    let register = || {
        let mut schema = Schema::<()>::new();
        schema.object::<Item>(|object| {
            for name in &names {
                object.field(name, |item: &Item, _: gurafi::NoArgs, _: &()| {
                    item.name.clone()
                });
            }
        });
        schema
    };

    for _ in 0..5 {
        std::hint::black_box(register());
    }
    let elapsed = time(iterations, || {
        std::hint::black_box(register());
    });
    report("register 500 fields", iterations, elapsed, FIELDS);
}

#[test]
#[ignore = "timing probe"]
fn probe_plugin_build_pass() {
    use gurafi::{FieldMeta, Plugin};

    struct MetadataPlugin;
    impl Plugin<()> for MetadataPlugin {
        fn field(&self, meta: &FieldMeta<'_>, field: &mut gurafi::gql::Field) {
            if meta.is_root {
                field.description = Some("root".into());
            }
        }
    }

    struct ResolvePlugin;
    impl Plugin<()> for ResolvePlugin {
        fn wrap_resolve(
            &self,
            _meta: &FieldMeta<'_>,
            next: gurafi::gql::ResolveFn,
        ) -> gurafi::gql::ResolveFn {
            next
        }
    }

    const FIELDS: usize = 500;
    let names: Vec<&'static str> = (0..FIELDS)
        .map(|index| Box::leak(format!("field{index}").into_boxed_str()) as &'static str)
        .collect();

    let register = |plugins: u8| {
        let mut schema = Schema::<()>::new();
        if plugins >= 1 {
            schema.plugin(MetadataPlugin);
        }
        if plugins >= 2 {
            schema.plugin(ResolvePlugin);
        }
        schema.object::<Item>(|object| {
            for name in &names {
                object.field(name, |item: &Item, _: gurafi::NoArgs, _: &()| {
                    item.name.clone()
                });
            }
        });
        schema.query(|query| {
            query.field("item", |_: gurafi::NoArgs, _: &()| Item {
                name: String::new(),
            });
        });
        schema
    };

    let iterations = 200;
    for plugins in [0_u8, 1, 2] {
        for _ in 0..5 {
            std::hint::black_box(register(plugins).build().unwrap());
        }
        let elapsed = time(iterations, || {
            std::hint::black_box(register(plugins).build().unwrap());
        });
        report(
            &format!("build 500 fields + {plugins} plugins"),
            iterations,
            elapsed,
            FIELDS,
        );
    }
}

#[test]
#[ignore = "timing probe"]
fn probe_register_dynamic_objects() {
    for types in [100_usize, 500] {
        let iterations = if types > 100 { 50 } else { 200 };
        for _ in 0..5 {
            std::hint::black_box(register_objects(types));
        }
        let elapsed = time(iterations, || {
            std::hint::black_box(register_objects(types));
        });
        report(
            &format!("register {types} runtime-named objects"),
            iterations,
            elapsed,
            types,
        );
    }
}
