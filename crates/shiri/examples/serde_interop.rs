// TODO: Nested value (Eg. uuid) that works

use shiri::{Json, Serializer2};
use uuid::Uuid;

// Designed to mimic the `uuid` crate as an example`
mod uuid {
    #[derive(Default)]
    pub struct Uuid([u8; 16]);

    impl serde::Serialize for Uuid {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut s = String::new();
            // Poor mans `hex` crate
            for byte in self.0.iter() {
                s.push_str(&format!("{:02x}", byte));
            }
            serializer.serialize_str(&s)
        }
    }
}

fn main() {
    let mut buf = Vec::new();
    let mut shiri = Json::new(&mut buf);
    demo(Serializer2::new(&mut shiri));
    println!("{}", String::from_utf8_lossy(&buf));
}

// TODO: Replace trait with struct
fn demo(serializer: Serializer2) {
    let uuid = Uuid::default();
    serializer.serialize_with_serde(&uuid).unwrap();
}
