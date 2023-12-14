use gurafu::{expose, FieldBuilder, Schema};

#[derive(Debug)]
pub struct Giraffe {
    name: String,
    birthday: String,
    height_in_meters: f32,
}

// The equivalent of using Pothos classes.
// https://pothos-graphql.dev/docs/guide/objects#using-classes
fn giraffe_schema(s: &mut Schema) {
    s.object_type::<Giraffe>("Giraffe")
        .description("Long necks, cool patterns, taller than you.")
        .field("name", expose!(Giraffe:name))
        .field(
            "age",
            // TODO: Rust can't infer the model type to the resolver closure. Can we trick it?
            FieldBuilder::new(|m: Giraffe| m.birthday.parse::<u16>().unwrap()),
        )
        .field("height", expose!(Giraffe:height_in_meters));
}

fn main() {
    let mut schema = <Schema>::new().with(giraffe_schema);

    schema.query_field("giraffe", || Giraffe {
        name: "Tony".into(),
        birthday: 10.to_string(),
        height_in_meters: 5.2,
    });

    todo!();
}
