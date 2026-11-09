//! Field, argument and directive rules.
//!
//! These are the rules that need to know a selection's parent type. Membership rules (fragment
//! names, reachability, cycles) live in [`super`]; value shape rules live in [`super::values`].

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::{
    ast::{
        Argument, Directive, DirectiveLocation, Field, FragmentDefinition, FragmentSpread,
        InlineFragment, SelectionSet,
    },
    error::{GraphQLError, Location},
    schema::Schema,
    types::{InputValue, Name, Type, TypeRef},
    validation::{
        values::{check_input_field_names, validate_value},
        visitor::{FieldContext, MetaField, Visitor},
    },
};

/// Every object type that can appear where `name` is expected.
fn possible_objects(schema: &Schema, name: &str) -> Vec<Name> {
    match schema.get_type(name) {
        Some(Type::Object(object)) => vec![object.name.clone()],
        Some(Type::Interface(_) | Type::Union(_)) => schema.possible_types(name),
        _ => Vec::new(),
    }
}

/// Whether two types have at least one concrete object type in common.
pub(crate) fn types_overlap(schema: &Schema, left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left = possible_objects(schema, left);
    if left.is_empty() {
        return false;
    }
    let right = possible_objects(schema, right);
    left.iter().any(|name| right.contains(name))
}

/// Which construct owns a list of arguments, used to phrase error messages.
#[derive(Clone, Copy, Debug)]
enum ArgumentOwner<'a> {
    Field { parent: &'a str, name: &'a str },
    Directive { name: &'a str },
}

fn unique_argument_names(provided: &[Argument<'_>], errors: &mut Vec<GraphQLError>) {
    let mut seen = crate::validation::NameSet::new();
    for argument in provided {
        if !seen.insert(argument.name) {
            errors.push(
                GraphQLError::new(format!(
                    "There can be only one argument named \"{}\".",
                    argument.name
                ))
                .at(argument.location),
            );
        }
        check_input_field_names(&argument.value, errors);
    }
}

fn check_arguments(
    schema: &Schema,
    definitions: &IndexMap<Name, InputValue>,
    provided: &[Argument<'_>],
    owner: ArgumentOwner<'_>,
    owner_location: Location,
    errors: &mut Vec<GraphQLError>,
) {
    for argument in provided {
        match crate::types::map_get(definitions, argument.name) {
            Some(definition) => validate_value(
                schema,
                &argument.value,
                &definition.ty,
                false,
                argument.location,
                errors,
            ),
            None => {
                let message = match owner {
                    ArgumentOwner::Field { parent, name } => format!(
                        "Unknown argument \"{}\" on field \"{parent}.{name}\".",
                        argument.name
                    ),
                    ArgumentOwner::Directive { name } => format!(
                        "Unknown argument \"{}\" on directive \"@{name}\".",
                        argument.name
                    ),
                };
                errors.push(GraphQLError::new(message).at(argument.location));
            }
        }
    }

    for definition in definitions.values() {
        if matches!(definition.ty, TypeRef::NonNull(_))
            && definition.default_value.is_none()
            && !provided
                .iter()
                .any(|argument| argument.name == &*definition.name)
        {
            let message = match owner {
                ArgumentOwner::Field { parent, name } => format!(
                    "Field \"{parent}.{name}\" argument \"{}\" of type \"{}\" is required, but it was not provided.",
                    definition.name, definition.ty
                ),
                ArgumentOwner::Directive { name } => format!(
                    "Directive \"@{name}\" argument \"{}\" of type \"{}\" is required, but it was not provided.",
                    definition.name, definition.ty
                ),
            };
            errors.push(GraphQLError::new(message).at(owner_location));
        }
    }
}

/// Check every directive applied in one location.
pub(crate) fn check_directives(
    schema: &Schema,
    directives: &[Directive<'_>],
    location: DirectiveLocation,
    errors: &mut Vec<GraphQLError>,
) {
    let mut seen = crate::validation::NameSet::new();
    for directive in directives {
        let Some(definition) = schema.get_directive(directive.name) else {
            errors.push(
                GraphQLError::new(format!("Unknown directive \"@{}\".", directive.name))
                    .at(directive.location),
            );
            continue;
        };

        if !definition.locations.contains(&location) {
            errors.push(
                GraphQLError::new(format!(
                    "Directive \"@{}\" may not be used on {}.",
                    directive.name,
                    location.as_str()
                ))
                .at(directive.location),
            );
        }

        if !definition.repeatable && !seen.insert(directive.name) {
            errors.push(
                GraphQLError::new(format!(
                    "The directive \"@{}\" can only be used once at this location.",
                    directive.name
                ))
                .at(directive.location),
            );
        }

        unique_argument_names(&directive.arguments, errors);
        check_arguments(
            schema,
            &definition.args,
            &directive.arguments,
            ArgumentOwner::Directive {
                name: directive.name,
            },
            directive.location,
            errors,
        );
    }
}

/// The visitor that checks fields, their arguments and their directives.
pub(crate) struct FieldRules<'a, 'b> {
    pub(crate) schema: &'a Schema,
    pub(crate) fragments: &'b HashMap<&'a str, &'a FragmentDefinition<'a>>,
    pub(crate) errors: &'b mut Vec<GraphQLError>,
}

impl<'a> FieldRules<'a, '_> {
    fn check_field_arguments(&mut self, field: &'a Field<'a>, context: &FieldContext<'a>) {
        unique_argument_names(&field.arguments, self.errors);

        let parent = context
            .parent_type
            .map(|parent| parent.name().as_ref())
            .unwrap_or("");

        if let Some(definition) = context.definition {
            check_arguments(
                self.schema,
                &definition.args,
                &field.arguments,
                ArgumentOwner::Field {
                    parent,
                    name: field.name,
                },
                field.location,
                self.errors,
            );
        } else if context.meta == Some(MetaField::Type) {
            for argument in &field.arguments {
                if argument.name == "name" {
                    validate_value(
                        self.schema,
                        &argument.value,
                        &TypeRef::string().non_null(),
                        false,
                        argument.location,
                        self.errors,
                    );
                } else {
                    self.errors.push(
                        GraphQLError::new(format!(
                            "Unknown argument \"{}\" on field \"{parent}.{}\".",
                            argument.name, field.name
                        ))
                        .at(argument.location),
                    );
                }
            }
            if !field
                .arguments
                .iter()
                .any(|argument| argument.name == "name")
            {
                self.errors.push(
                    GraphQLError::new(format!(
                        "Field \"{parent}.{}\" argument \"name\" of type \"String!\" is required, but it was not provided.",
                        field.name
                    ))
                    .at(field.location),
                );
            }
        } else if context.meta.is_some() {
            // `__typename` and `__schema` accept no arguments.
            for argument in &field.arguments {
                self.errors.push(
                    GraphQLError::new(format!(
                        "Unknown argument \"{}\" on field \"{parent}.{}\".",
                        argument.name, field.name
                    ))
                    .at(argument.location),
                );
            }
        }
    }
}

impl<'a> Visitor<'a> for FieldRules<'a, '_> {
    fn field(&mut self, field: &'a Field<'a>, context: &FieldContext<'a>) {
        if context.definition.is_none() && context.meta.is_none() {
            if let Some(parent) = context.parent_type {
                self.errors.push(
                    GraphQLError::new(format!(
                        "Cannot query field \"{}\" on type \"{}\".",
                        field.name,
                        parent.name()
                    ))
                    .at(field.location),
                );
            }
        }

        if let Some(return_type) = context.return_type {
            let named = self.schema.get_type(return_type.named_type());
            let is_leaf = named.is_some_and(|ty| ty.is_leaf());
            let is_composite = named.is_some_and(|ty| ty.is_composite())
                // The introspection types are injected by the schema builder. If a schema
                // somehow lacks them, `__schema` and `__type` are still composite by
                // definition, so their selection sets must not be treated as leaf selections.
                || (named.is_none()
                    && matches!(
                        context.meta,
                        Some(MetaField::Schema | MetaField::Type)
                    ));

            if is_leaf {
                if field.selection_set.is_some() {
                    self.errors.push(
                        GraphQLError::new(format!(
                            "Field \"{}\" must not have a selection since type \"{return_type}\" has no subfields.",
                            field.name
                        ))
                        .at(field.location),
                    );
                }
            } else if is_composite
                && field
                    .selection_set
                    .as_ref()
                    .is_none_or(SelectionSet::is_empty)
            {
                self.errors.push(
                    GraphQLError::new(format!(
                        "Field \"{}\" of type \"{return_type}\" must have a selection of subfields. Did you mean \"{} {{ ... }}\"?",
                        field.name, field.name
                    ))
                    .at(field.location),
                );
            }
        }

        self.check_field_arguments(field, context);
        check_directives(
            self.schema,
            &field.directives,
            DirectiveLocation::Field,
            self.errors,
        );
    }

    fn inline_fragment(&mut self, fragment: &'a InlineFragment<'a>, parent: Option<&'a Type>) {
        if let (Some(parent), Some(condition)) = (parent, fragment.type_condition) {
            if let Some(condition_type) = self.schema.get_type(condition) {
                if condition_type.is_composite()
                    && !types_overlap(self.schema, parent.name(), condition)
                {
                    self.errors.push(
                        GraphQLError::new(format!(
                            "Fragment cannot be spread here as objects of type \"{}\" can never be of type \"{condition}\".",
                            parent.name()
                        ))
                        .at(fragment.location),
                    );
                }
            }
        }
        check_directives(
            self.schema,
            &fragment.directives,
            DirectiveLocation::InlineFragment,
            self.errors,
        );
    }

    fn fragment_spread(&mut self, spread: &'a FragmentSpread<'a>, parent: Option<&'a Type>) {
        match self.fragments.get(spread.name) {
            None => {
                self.errors.push(
                    GraphQLError::new(format!("Unknown fragment \"{}\".", spread.name))
                        .at(spread.location),
                );
            }
            Some(fragment) => {
                if let Some(parent) = parent {
                    if let Some(condition) = self.schema.get_type(fragment.type_condition) {
                        if condition.is_composite()
                            && !types_overlap(self.schema, parent.name(), fragment.type_condition)
                        {
                            self.errors.push(
                                GraphQLError::new(format!(
                                    "Fragment \"{}\" cannot be spread here as objects of type \"{}\" can never be of type \"{}\".",
                                    spread.name,
                                    parent.name(),
                                    fragment.type_condition
                                ))
                                .at(spread.location),
                            );
                        }
                    }
                }
            }
        }

        check_directives(
            self.schema,
            &spread.directives,
            DirectiveLocation::FragmentSpread,
            self.errors,
        );
    }
}
