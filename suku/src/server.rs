use core::fmt;
use std::collections::HashMap;

use gql::{NamedOutputType, ObjectType, OutputType, Schema, Thunk};
use graphql_parser::query::{Definition, Document, OperationDefinition, Selection, SelectionSet};

use crate::Value;

// TODO: Can the key of this map hold a lifetime??? -> It would require us to take care of serialize in `Server::execute` which seems likely.
type Response = HashMap<String, Value>;

pub struct Server {
    schema: Schema,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server").finish()
    }
}

impl Server {
    pub fn new(schema: Schema) -> Self {
        Self { schema }
    }

    // TODO: Proper error handling + serializable error
    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub async fn execute(&self, document: String) -> Result<Response, ()> {
        let query = parse_query(&document);
        // println!("{:#?}", query);

        let mut response = Response::new();
        resolve(&self.schema, &mut response, query);
        Ok(response)
    }
}

#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn parse_query(query: &str) -> graphql_parser::query::Document<&str> {
    graphql_parser::parse_query::<&str>(query).unwrap()
}

// TODO: Handle introspection
// TODO: Handle fragments
// TODO: Catch panics
// TODO: Deal with `__typename`
#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn resolve<'a>(schema: &Schema, response: &mut Response, document: Document<'a, &'a str>) {
    for def in document.definitions {
        match def {
            Definition::Operation(op) => {
                println!("{:#?}", op);

                match op {
                    OperationDefinition::SelectionSet(selection) => {
                        // println!("{:#?}", selection);
                        resolve_selection_set(&schema.query, response, selection);
                    }
                    OperationDefinition::Query(_) => todo!(),
                    OperationDefinition::Mutation(_) => todo!(),
                    OperationDefinition::Subscription(_) => todo!(),
                }
            }
            Definition::Fragment(_) => todo!(),
        }
    }
}

#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn resolve_selection_set<'a>(
    object_ty: &ObjectType,
    response: &mut Response,
    selection: SelectionSet<'a, &'a str>,
) {
    for item in selection.items {
        match item {
            Selection::Field(field) => {
                debug_assert!(field.arguments.is_empty(), "todo: implement arguments");
                debug_assert!(field.directives.is_empty(), "todo: implement directives");

                let field_name = field.alias.unwrap_or_else(|| field.name);

                let field_def = match &object_ty.fields {
                    // TODO: Cache this between execute calls
                    Thunk::Lazy(func) => todo!(),
                    Thunk::Eager(fields) => fields,
                }
                .get(field_name)
                .unwrap_or_else(|| panic!("field not found: {:?}", field_name)); // TODO: Error instead

                let field_ty = match &field_def.ty {
                    OutputType::Nullable(ty) => ty,
                    OutputType::NonNullable(ty) => ty,
                    OutputType::List(_) => todo!(),
                };

                match field_ty {
                    NamedOutputType::Scalar(scalar) => {
                        if let Some(resolve) = field_def.resolve {
                            response.insert(field.name.into(), Value::String((resolve)()));
                        } else {
                            // TODO: Error instead
                            response.insert(
                                field.name.into(),
                                Value::String("Missing response".into()),
                            );
                        }
                    }
                    NamedOutputType::Object(inner_object_ty) => {
                        let mut nested_response = HashMap::new();
                        resolve_selection_set(
                            inner_object_ty,
                            &mut nested_response,
                            field.selection_set,
                        );
                        response.insert(field.name.into(), Value::Object(nested_response));
                    }
                }
            }
            Selection::FragmentSpread(_) => todo!(),
            Selection::InlineFragment(_) => todo!(),
        }
    }
}
