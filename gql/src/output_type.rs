use std::borrow::Cow;

/// https://github.com/graphql/graphql-js/blob/688ee2f2a2f938e80bcf3808bc435d3d87a3ff3e/src/type/definition.ts#L225
pub enum OutputType {
    Nullable(NamedOutputType),
    NonNullable(NamedOutputType),
    List(NamedOutputType),
}

/// https://github.com/graphql/graphql-js/blob/688ee2f2a2f938e80bcf3808bc435d3d87a3ff3e/src/type/definition.ts#L440
pub enum NamedOutputType {
    Scalar(ScalarType),
    // Object(),
    // Interface(),
    // Union(),
    // Enum(),
}

/// https://github.com/graphql/graphql-js/blob/688ee2f2a2f938e80bcf3808bc435d3d87a3ff3e/src/type/definition.ts#L545
pub struct ScalarType {
    name: Cow<'static, str>,
    description: Option<Cow<'static, str>>,
    // specifiedByURL: Maybe<string>;
    // serialize: GraphQLScalarSerializer<TExternal>;
    // parseValue: GraphQLScalarValueParser<TInternal>;
    // parseLiteral: GraphQLScalarLiteralParser<TInternal>;
    // extensions: Readonly<GraphQLScalarTypeExtensions>;
    // astNode: Maybe<ScalarTypeDefinitionNode>;
    // extensionASTNodes: ReadonlyArray<ScalarTypeExtensionNode>;
}
