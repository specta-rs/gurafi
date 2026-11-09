use shiri::{Json, Serializer};

fn main() {
    let mut buf = Vec::new();
    let mut shiri = Json::new(&mut buf);
    demo(&mut shiri);
    println!("{}", String::from_utf8_lossy(&buf));
}

// TODO: Replace trait with struct
// fn demo(serializer: &mut dyn Serializer) {
//     serializer.serialize_str("hello").unwrap();
// }

// TODO: Replace trait with struct
fn demo(serializer: &mut dyn Serializer) {
    let mut map = serializer.serialize_map().unwrap();

    map.serialize_entry("a".to_string())
        .unwrap()
        .serialize_str("a's value")
        .unwrap();

    let mut b_map = map
        .serialize_entry("b".to_string())
        .unwrap()
        .serialize_map()
        .unwrap();
    b_map
        .serialize_entry("bb".to_string())
        .unwrap()
        .serialize_str("bb's value")
        .unwrap();
    b_map.end(); // TODO: Detect and error if the user forgets this

    let mut c_map = map
        .serialize_entry("c".to_string())
        .unwrap()
        .serialize_map()
        .unwrap();
    let mut cc_map = c_map
        .serialize_entry("cc".to_string())
        .unwrap()
        .serialize_map()
        .unwrap();
    cc_map
        .serialize_entry("ccc")
        .unwrap()
        .serialize_str("cc's value")
        .unwrap();
    // TODO: What if we end the maps out of order???
    cc_map.end();
    c_map.end(); // TODO: Detect and error if the user forgets this

    map.end();
}
