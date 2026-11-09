//! Review repro: converting a JSON variables object with many keys is quadratic, because
//! `gql::Object` is a linear `SmallVec` and `suku` inserts every key one by one.
//!

use std::time::Instant;

use suku::GraphQLRequest;

fn body(keys: usize) -> String {
    let mut out = String::from("{\"query\":\"{ ping }\",\"variables\":{");
    for index in 0..keys {
        if index != 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}k\":0", base26(index)));
    }
    out.push_str("}}");
    out
}

fn base26(mut index: usize) -> String {
    let mut out = String::new();
    loop {
        out.push((b'a' + (index % 26) as u8) as char);
        index /= 26;
        if index == 0 {
            return out.chars().rev().collect();
        }
    }
}

fn convert(keys: usize) -> std::time::Duration {
    let payload = body(keys);
    let parsed: GraphQLRequest = serde_json::from_str(&payload).expect("valid request");
    let start = Instant::now();
    let object = parsed.variables_object().expect("converts");
    let elapsed = start.elapsed();
    assert_eq!(object.map_or(0, |object| object.len()), keys);
    elapsed
}

#[test]
fn variable_object_conversion_scaling() {
    // Four times the keys: linear conversion goes up ~4x, quadratic goes up ~16x.
    let small = convert(4_000);
    let large = convert(16_000);
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    println!("4k keys: {small:?}, 16k keys: {large:?}, ratio {ratio:.1}");
    assert!(
        ratio < 8.0,
        "16k keys took {ratio:.1}x as long as 4k keys ({large:?} vs {small:?}): the conversion is \
         quadratic, so a ~1 MiB variables object costs seconds of CPU"
    );
}
