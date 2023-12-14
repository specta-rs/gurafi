use crate::ObjectType;

/// https://graphql.org/graphql-js/type/#graphqlschema
#[derive(Debug)]
pub struct Schema {
    pub query: ObjectType,
    pub mutation: Option<ObjectType>,
}
