use core::fmt;
use std::collections::HashMap;

use gql::{Schema, Thunk};
use graphql_parser::query::{Definition, Document, OperationDefinition, Selection, SelectionSet};

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
    pub async fn execute(&self, document: String) -> Result<HashMap<String, String>, ()> {
        let query = parse_query(&document);
        // println!("{:#?}", query);

        let mut response = HashMap::new();
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
pub fn resolve<'a>(
    schema: &Schema,
    response: &mut HashMap<String, String>,
    document: Document<'a, &'a str>,
) {
    for def in document.definitions {
        match def {
            Definition::Operation(op) => {
                println!("{:#?}", op);

                match op {
                    OperationDefinition::SelectionSet(selection) => {
                        // println!("{:#?}", selection);
                        resolve_selection_set(schema, response, selection);
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
    schema: &Schema,
    response: &mut HashMap<String, String>,
    selection: SelectionSet<'a, &'a str>,
) {
    for item in selection.items {
        match item {
            Selection::Field(field) => {
                debug_assert!(field.alias.is_none(), "todo: implement alias");
                debug_assert!(field.arguments.is_empty(), "todo: implement arguments");
                debug_assert!(field.directives.is_empty(), "todo: implement directives");
                debug_assert!(
                    field.selection_set.items.is_empty(),
                    "todo: implement nested selection sets"
                );

                let field_def = match &schema.query.fields {
                    // TODO: Cache this between execute calls
                    Thunk::Lazy(func) => todo!(),
                    Thunk::Eager(fields) => fields,
                }
                .get(field.name)
                .unwrap(); // TODO: Error instead

                if let Some(resolve) = field_def.resolve {
                    response.insert(field.name.to_string(), (resolve)());
                } else {
                    // TODO: Error instead
                    response.insert(field.name.to_string(), "Missing response".into());
                }
            }
            Selection::FragmentSpread(_) => todo!(),
            Selection::InlineFragment(_) => todo!(),
        }
    }
}
