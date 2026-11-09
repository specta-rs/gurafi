use gql::serializer::Serializer;

fn serde_interop<T: serde::Serialize>(serializer: Serializer, t: &T) {
    serializer.serialize_serde(&t)
}
