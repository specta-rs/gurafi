use gql::serializer::OwnedSerializer;

#[test]
fn serializes_map_through_owned_serializer() {
    let mut buf = Vec::new();

    {
        let mut serde = serde_json::Serializer::new(&mut buf);
        let mut serializer = OwnedSerializer::new(&mut serde);

        let mut map = serializer.serialize().serialize_map(Some(3)).unwrap();
        map.serialize_entry(&"hello", &"world").unwrap();
        map.serialize_entry(&"answer", &42).unwrap();
        map.serialize_entry(&"list", &vec![1, 2, 3]).unwrap();
        map.end();
    }

    assert_eq!(
        std::str::from_utf8(&buf).unwrap(),
        r#"{"hello":"world","answer":42,"list":[1,2,3]}"#
    );
}

#[test]
fn serializes_seq_through_owned_serializer() {
    let mut buf = Vec::new();

    {
        let mut serde = serde_json::Serializer::new(&mut buf);
        let mut serializer = OwnedSerializer::new(&mut serde);

        let mut seq = serializer.serialize().serialize_seq(Some(3)).unwrap();
        seq.serialize_element(&1).unwrap();
        seq.serialize_element(&"two").unwrap();
        seq.serialize_element(&true).unwrap();
        seq.end();
    }

    assert_eq!(std::str::from_utf8(&buf).unwrap(), r#"[1,"two",true]"#);
}

#[test]
fn serializes_scalar_into_map_value_slot() {
    let mut buf = Vec::new();

    {
        let mut serde = serde_json::Serializer::new(&mut buf);
        let mut serializer = OwnedSerializer::new(&mut serde);

        let mut map = serializer.serialize().serialize_map(Some(1)).unwrap();
        let value = map.serialize_entry_serializer(&"hello").unwrap();
        value.serialize_str("world");
        map.end();
    }

    assert_eq!(std::str::from_utf8(&buf).unwrap(), r#"{"hello":"world"}"#);
}
