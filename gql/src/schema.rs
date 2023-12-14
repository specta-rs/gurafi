use crate::ObjectType;

/// https://graphql.org/graphql-js/type/#graphqlschema
pub struct Schema {
    query: ObjectType,
    mutation: Option<ObjectType>,
}
