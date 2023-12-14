use graphql_parser::query::Field;

pub trait Resolver {
    fn resolve<'a>(&self, fields: &'a Field<'a, &'a str>);
}
