use core::fmt;

use gql::{
    serializer::{OwnedSerializer, Serializer},
    GraphQLResolveInfo, NamedOutputType, ObjectType, OutputType, ResolveFnArgs, Schema, Thunk,
};
use graphql_parser::query::{Definition, Document, OperationDefinition, Selection, SelectionSet};

use crate::introspection;

pub struct Server {
    schema: Schema,
    enabled_introspection: bool,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server").finish()
    }
}

impl Server {
    pub fn new(schema: Schema) -> Self {
        Self {
            schema,
            enabled_introspection: false,
        }
    }

    pub fn introspection(mut self, enabled: bool) -> Self {
        self.enabled_introspection = enabled;
        self
    }

    // TODO: Have an execute method that doesn't do streaming

    // TODO: Proper error handling + serializable error
    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub async fn execute<S: gql::serializer::AnyOwnedSerializer>(
        &self,
        mut serializer: OwnedSerializer<S>,
        document: String,
    ) -> Result<(), ()> // TODO: Outer result???
    {
        let document = &document;
        let query = parse_query(document);

        resolve(&self, &self.schema, serializer.serialize(), query);

        Ok(())
    }

    // TODO: Streaming `execute`
}

#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn parse_query(query: &str) -> graphql_parser::query::Document<&str> {
    graphql_parser::parse_query::<&str>(query).unwrap()
}

#[cfg_attr(feature = "tracing", tracing::instrument(skip(serializer)))]
pub fn resolve<'a>(
    server: &Server,
    schema: &Schema,
    serializer: Serializer,
    document: Document<'a, &'a str>,
) {
    let mut serializer = Some(serializer);

    for def in document.definitions {
        match def {
            Definition::Operation(op) => {
                // TODO: I am not sure what the GraphQL spec says should be done here so this will suffice for now.
                let serializer = serializer
                    .take()
                    .expect("todo: error because only one top level query is allowed");

                match op {
                    OperationDefinition::SelectionSet(selection) => {
                        resolve_selection_set(server, &schema.query, serializer, selection);
                    }
                    OperationDefinition::Query(query) => {
                        debug_assert!(
                            query.variable_definitions.is_empty(),
                            "todo: implement variable definitions"
                        );
                        debug_assert!(query.directives.is_empty(), "todo: implement directives");

                        // TODO: `query.name` does it need to be handled???

                        resolve_selection_set(
                            server,
                            &schema.query,
                            serializer,
                            query.selection_set,
                        );
                    }
                    OperationDefinition::Mutation(_) => todo!(),
                    OperationDefinition::Subscription(_) => todo!(),
                }
            }
            Definition::Fragment(_) => todo!(),
        }
    }
}

// #[cfg_attr(feature = "tracing", tracing::instrument(skip(serializer)))]
pub fn resolve_selection_set<'a>(
    server: &Server,
    object_ty: &ObjectType,
    serializer: Serializer,
    selection: SelectionSet<'a, &'a str>,
) {
    let mut serializer = serializer
        .serialize_map(Some(selection.items.len()))
        .unwrap(); // TODO: Error handling

    for item in selection.items {
        match item {
            Selection::Field(field) => {
                debug_assert!(field.arguments.is_empty(), "todo: implement arguments");
                debug_assert!(field.directives.is_empty(), "todo: implement directives");

                let field_name = field.alias.unwrap_or_else(|| field.name);

                let fields = match &object_ty.fields {
                    // TODO: Cache this between execute calls
                    Thunk::Lazy(func) => todo!(),
                    Thunk::Eager(fields) => fields,
                };

                let field_def = match fields.get(field_name) {
                    Some(field_def) => field_def,
                    None => {
                        if server.enabled_introspection {
                            if field_name == "__schema" {
                                let schema = introspection::schema(); // TODO: Cache the output of this

                                // let mut inner_response = HashMap::new();
                                // resolve_selection_set(
                                //     server,
                                //     &schema,
                                //     &mut inner_response,
                                //     field.selection_set,
                                // );

                                // response.insert(field.name.into(), Value::Object(inner_response));

                                serializer.serialize_entry(&field.name, &"todo").unwrap(); // TODO: Error handling
                                continue;
                            }
                        }

                        panic!("field not found: {:?}", field_name); // TODO: Error instead
                    }
                };

                let field_ty = match &field_def.ty {
                    OutputType::Nullable(ty) => ty,
                    OutputType::NonNullable(ty) => ty,
                    OutputType::List(_) => todo!(),
                };

                match field_ty {
                    NamedOutputType::Scalar(scalar) => {
                        assert!(
                            field.selection_set.items.is_empty(),
                            "todo: error because scalar fields can't have selection sets"
                        );

                        if let Some(resolve) = &field_def.resolve {
                            let serializer =
                                serializer.serialize_entry_serializer(&field.name).unwrap();

                            let args = ResolveFnArgs {
                                context: &(), // TODO: Get this from the user
                                info: GraphQLResolveInfo {
                                    field_name: field.name,
                                },
                            };

                            match resolve {
                                gql::ResolveFn::Ptr(f) => (f)(args, serializer),
                                gql::ResolveFn::Boxed(f) => (f)(args, serializer),
                                // TODO: Make sure multiple async are run at the same time. Basically don't `await`, queue onto the runtime
                                gql::ResolveFn::PtrAsync(f) => todo!(),
                                gql::ResolveFn::BoxedAsync(_) => todo!(),
                            };

                            // TODO: Error if the user forgets to use `serializer`
                        } else {
                            // TODO: Error instead
                            serializer
                                .serialize_entry(&field.name, &"Missing response")
                                .unwrap(); // TODO: Error handling
                        }
                    }
                    NamedOutputType::Object(inner_object_ty) => {
                        // TODO: This is broken due to Serde limitations.
                        // Gonna try and make an alternative to Serde and integrate it to fix this!

                        let serializer =
                            serializer.serialize_entry_serializer(&field.name).unwrap();

                        resolve_selection_set(
                            server,
                            inner_object_ty,
                            serializer,
                            field.selection_set,
                        );
                    }
                }
            }
            Selection::FragmentSpread(_) => todo!(),
            Selection::InlineFragment(_) => todo!(),
        }
    }

    serializer.end();
}
