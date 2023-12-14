use core::fmt;
use std::collections::HashMap;

use graphql_parser::query::{Definition, OperationDefinition, Selection};

pub struct Server {
    // query: Resolver,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server").finish()
    }
}

impl Server {
    pub fn new() -> Self {
        Self {}
    }

    // TODO: Proper error handling + serializable error
    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub async fn execute(&self, document: String) -> Result<HashMap<String, &'static str>, ()> {
        let query = parse_query(&document);
        println!("{:#?}", query);

        // TODO: Handle introspection
        // TODO: Handle fragments
        // TODO: Catch panics
        // TODO: Deal with `__typename`

        let mut response = HashMap::new();

        for def in query.definitions {
            match def {
                Definition::Operation(op) => {
                    println!("{:#?}", op);

                    match op {
                        OperationDefinition::SelectionSet(selection) => {
                            println!("{:#?}", selection);

                            for item in selection.items {
                                match item {
                                    Selection::Field(field) => {
                                        debug_assert!(
                                            field.alias.is_none(),
                                            "todo: implement alias"
                                        );
                                        debug_assert!(
                                            field.arguments.is_empty(),
                                            "todo: implement arguments"
                                        );
                                        debug_assert!(
                                            field.directives.is_empty(),
                                            "todo: implement directives"
                                        );
                                        debug_assert!(
                                            field.selection_set.items.is_empty(),
                                            "todo: implement nested selection sets"
                                        );

                                        response.insert(field.name.to_string(), "Hello World");
                                    }
                                    Selection::FragmentSpread(_) => todo!(),
                                    Selection::InlineFragment(_) => todo!(),
                                }
                            }
                        }
                        OperationDefinition::Query(_) => todo!(),
                        OperationDefinition::Mutation(_) => todo!(),
                        OperationDefinition::Subscription(_) => todo!(),
                    }
                }
                Definition::Fragment(_) => todo!(),
            }
        }

        Ok(response)
    }
}

#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn parse_query(query: &str) -> graphql_parser::query::Document<&str> {
    graphql_parser::parse_query::<&str>(query).unwrap()
}
