use shiri::{serde::SerdeInterop, Serializer};

fn main() {
    let mut buf = Vec::new();
    let mut json = serde_json::Serializer::new(&mut buf);
    let mut serializer = SerdeInterop::new(&mut json);
    demo(&mut serializer);
    println!("{}", String::from_utf8_lossy(&buf));
}

// TODO: Replace trait with struct
fn demo(serializer: &mut dyn Serializer) {
    let mut map = serializer.serialize_map().unwrap();
    map.serialize_entry("hello")
        .unwrap()
        .serialize_str("hello")
        .unwrap();
    map.end()
}
