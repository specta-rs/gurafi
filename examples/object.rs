use gurafu::{expose, FieldBuilder, Model, ModelBuilder, Schema};

#[derive(Debug)]
pub struct Giraffe {
    name: String,
    birthday: String,
    height_in_meters: f32,
}

// The equivalent of using Pothos classes.
// https://pothos-graphql.dev/docs/guide/objects#using-classes
impl Model for Giraffe {
    fn define(t: ModelBuilder<Self>) -> ModelBuilder<Self> {
        t.name("Giraffe")
            .description("Long necks, cool patterns, taller than you.")
            .field("name", expose!(Giraffe:name))
            .field(
                "age",
                // TODO: Rust can't infer the model type to the resolver closure. Can we trick it?
                FieldBuilder::new(|m: Giraffe| m.birthday.parse::<u16>().unwrap()),
            )
            .field("height", expose!(Giraffe:height_in_meters))

        // TODO: How do we get the context type in here???
        // TODO: I assume field names and descriptions, directives, etc
    }
}

fn main() {
    let schema = <Schema>::new().query_field("giraffe", || Giraffe {
        name: "Tony".into(),
        birthday: 10.to_string(),
        height_in_meters: 5.2,
    });

    todo!();
}
