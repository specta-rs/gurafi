//! A small giraffe API built with `gurafi` and served by `suku`.

use gurafi::{Enum, Input, NoArgs, Object, Schema};

/// A long-necked animal.
#[derive(Clone, Debug, Object)]
pub struct Giraffe {
    pub name: String,
    pub height_meters: f64,
    pub species: Species,
}

/// The species of giraffe in the demo herd.
#[derive(Clone, Copy, Debug, Enum)]
pub enum Species {
    Masai,
    Reticulated,
    Rothschild,
}

/// Selects one giraffe by name.
#[derive(Input)]
pub struct GiraffeArgs {
    pub name: String,
}

/// Pagination for the herd.
#[derive(Input)]
pub struct GiraffesArgs {
    #[graphql(default = 10)]
    pub limit: i32,
    #[graphql(default = 0)]
    pub offset: i32,
}

/// Everything a resolver can see about the caller.
#[derive(Clone, Debug)]
pub struct AppContext {
    pub user: Option<User>,
}

/// An authenticated user.
#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
}

impl AppContext {
    /// A request with no bearer token.
    #[must_use]
    pub const fn anonymous() -> Self {
        Self { user: None }
    }

    /// A request authenticated as `name`.
    #[must_use]
    pub fn authenticated(name: impl Into<String>) -> Self {
        Self {
            user: Some(User { name: name.into() }),
        }
    }
}

fn herd() -> Vec<Giraffe> {
    vec![
        Giraffe {
            name: "Melman".to_owned(),
            height_meters: 5.2,
            species: Species::Masai,
        },
        Giraffe {
            name: "Tony".to_owned(),
            height_meters: 4.8,
            species: Species::Reticulated,
        },
        Giraffe {
            name: "Longneck".to_owned(),
            height_meters: 6.1,
            species: Species::Rothschild,
        },
    ]
}

/// Build the demo schema.
#[must_use]
pub fn schema() -> gql::Schema {
    let mut schema = Schema::<AppContext>::new();
    schema.description("A tiny giraffe API served by suku and Axum.");

    schema.object::<Giraffe>(|object| {
        object.description("Long necks, cool patterns, taller than you.");
        object.expose(gurafi::expose!(Giraffe::name));
        object.expose(gurafi::expose!(Giraffe::height_meters as "heightMeters"));
        object.expose(gurafi::expose!(Giraffe::species));
    });

    schema.query(|query| {
        query.field("giraffe", |args: GiraffeArgs, _: &AppContext| {
            herd()
                .into_iter()
                .find(|giraffe| giraffe.name.eq_ignore_ascii_case(&args.name))
        });

        query.field("giraffes", |args: GiraffesArgs, _: &AppContext| {
            let offset = usize::try_from(args.offset).unwrap_or(0);
            let limit = usize::try_from(args.limit).unwrap_or(0).min(100);
            herd()
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>()
        });

        query
            .field("whoami", |_: NoArgs, context: &AppContext| {
                context.user.as_ref().map(|user| user.name.clone())
            })
            .auth(|context: &AppContext| context.user.is_some())
            .auth_error("Send `Authorization: Bearer secret` to identify yourself.");
    });

    schema.build().expect("the demo schema is valid")
}
