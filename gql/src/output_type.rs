use std::borrow::Cow;

use crate::ObjectType;

/// https://github.com/graphql/graphql-js/blob/688ee2f2a2f938e80bcf3808bc435d3d87a3ff3e/src/type/definition.ts#L225
#[derive(Debug)]
pub enum OutputType {
    Nullable(NamedOutputType),
    NonNullable(NamedOutputType),
    List(NamedOutputType),
}

/// https://github.com/graphql/graphql-js/blob/688ee2f2a2f938e80bcf3808bc435d3d87a3ff3e/src/type/definition.ts#L440
#[derive(Debug)]
pub enum NamedOutputType {
    Scalar(ScalarType),
    Object(ObjectType),
    // Interface(),
    // Union(),
    // Enum(),
}

impl From<ScalarType> for NamedOutputType {
    fn from(v: ScalarType) -> Self {
        NamedOutputType::Scalar(v)
    }
}

impl From<ObjectType> for NamedOutputType {
    fn from(v: ObjectType) -> Self {
        NamedOutputType::Object(v)
    }
}

/// https://github.com/graphql/graphql-js/blob/688ee2f2a2f938e80bcf3808bc435d3d87a3ff3e/src/type/definition.ts#L545
#[derive(Debug)]
pub struct ScalarType {
    pub name: Cow<'static, str>,
    pub description: Option<Cow<'static, str>>,
    // specifiedByURL: Maybe<string>;
    // serialize: GraphQLScalarSerializer<TExternal>;
    // parseValue: GraphQLScalarValueParser<TInternal>;
    // parseLiteral: GraphQLScalarLiteralParser<TInternal>;
    // extensions: Readonly<GraphQLScalarTypeExtensions>;
    // astNode: Maybe<ScalarTypeDefinitionNode>;
    // extensionASTNodes: ReadonlyArray<ScalarTypeExtensionNode>;
}

impl ScalarType {
    // TODO: Is this a good idea or too high level???

    pub fn string() -> Self {
        Self {
            name: "String".into(),
            description: None,
        }
    }
}
