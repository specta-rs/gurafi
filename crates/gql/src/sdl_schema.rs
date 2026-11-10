//! Build a [`Schema`] from a schema definition language document.
//!
//! SDL-described schemas have no resolvers; they are useful for tooling, compatibility checks,
//! introspection snapshots and code generation. Server code normally builds schemas with the
//! builder (or with `gurafi`) and attaches resolvers.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::{
    Value,
    ast::{self, Definition, TypeDefinitionKind, TypeExtensionKind, TypeSystemDefinition},
    error::GraphQLError,
    schema::{DirectiveOwner, NamedOwner, NamedPath, Schema, SchemaBuilder, SchemaError},
    types::{
        AppliedDirective, DirectiveDefinition, EnumType, EnumValue, Field, InputObjectType,
        InputValue, InterfaceType, Name, ObjectType, ScalarType, Type, TypeRef, UnionType,
    },
};

/// Build a schema from GraphQL SDL.
///
/// # Errors
///
/// Returns parse and schema validation errors. The returned error contains every problem found,
/// not just the first one.
pub fn schema_from_sdl(source: &str) -> Result<Schema, SchemaError> {
    let document = crate::parser::parse_schema(source).map_err(SchemaError::from)?;

    let directive_errors = check_sdl_directives(&document);
    if !directive_errors.is_empty() {
        return Err(SchemaError {
            errors: directive_errors,
        });
    }

    let mut definitions = Definitions::default();
    // Definitions, schema definitions and directive definitions are order-independent. Apply
    // extensions in a second pass so `extend type Query` may appear before `type Query`.
    for definition in &document.definitions {
        match definition {
            Definition::TypeSystem(
                definition @ (TypeSystemDefinition::Type(_)
                | TypeSystemDefinition::Schema(_)
                | TypeSystemDefinition::Directive(_)),
            ) => definitions.add(definition),
            Definition::TypeSystem(_) => {}
            Definition::Operation(_) | Definition::Fragment(_) => {
                definitions.errors.push(GraphQLError::new(
                    "Executable definitions are not allowed in a schema document.",
                ));
            }
        }
    }
    for definition in &document.definitions {
        if matches!(
            definition,
            Definition::TypeSystem(
                TypeSystemDefinition::TypeExtension(_) | TypeSystemDefinition::SchemaExtension(_)
            )
        ) {
            if let Definition::TypeSystem(definition) = definition {
                definitions.add(definition);
            }
        }
    }

    let schema = definitions.finish()?;

    // Directive arguments are checked against the built schema, so custom scalars, enums and
    // input objects are coerced exactly the way they are for field arguments.
    let mut errors = validate_directive_values(&schema, &document);
    // Non-repeatable directives may be applied once on a definition and once on an extension;
    // those applications only meet in the schema model, so they are checked after the build.
    check_model_directives(&schema, &mut errors);
    if errors.is_empty() {
        Ok(schema)
    } else {
        Err(SchemaError { errors })
    }
}

/// The directives the specification defines. User directives are added from the document.
fn check_sdl_directives(document: &ast::Document<'_>) -> Vec<GraphQLError> {
    use crate::ast::DirectiveLocation as Location;

    struct Spec {
        locations: Vec<Location>,
        /// Argument name and whether it is required.
        args: Vec<(String, bool)>,
        repeatable: bool,
    }

    let builtin = |locations: &[Location], args: &[(&str, bool)]| Spec {
        locations: locations.to_vec(),
        args: args
            .iter()
            .map(|(name, required)| ((*name).to_owned(), *required))
            .collect(),
        repeatable: false,
    };
    let mut specs: FxHashMap<&str, Spec> = FxHashMap::default();
    specs.insert(
        "skip",
        builtin(
            &[
                Location::Field,
                Location::FragmentSpread,
                Location::InlineFragment,
            ],
            &[("if", true)],
        ),
    );
    specs.insert(
        "include",
        builtin(
            &[
                Location::Field,
                Location::FragmentSpread,
                Location::InlineFragment,
            ],
            &[("if", true)],
        ),
    );
    specs.insert(
        "deprecated",
        builtin(
            &[
                Location::FieldDefinition,
                Location::ArgumentDefinition,
                Location::InputFieldDefinition,
                Location::EnumValue,
                Location::DirectiveDefinition,
            ],
            &[("reason", false)],
        ),
    );
    specs.insert(
        "specifiedBy",
        builtin(&[Location::Scalar], &[("url", true)]),
    );
    specs.insert("oneOf", builtin(&[Location::InputObject], &[]));

    // The edges of the directive-reference graph, used to reject a directive that applies
    // itself (directly or transitively) to one of its own arguments.
    let mut references: FxHashMap<&str, Vec<&str>> = FxHashMap::default();

    for definition in &document.definitions {
        if let Definition::TypeSystem(TypeSystemDefinition::Directive(directive)) = definition {
            let arguments: Vec<(String, bool)> = directive
                .arguments
                .iter()
                .map(|argument| {
                    (
                        argument.name.to_owned(),
                        matches!(argument.ty, ast::Type::NonNull(_))
                            && argument.default_value.is_none(),
                    )
                })
                .collect();
            let mut edges = Vec::new();
            for argument in &directive.arguments {
                for applied in &argument.directives {
                    edges.push(applied.name);
                }
            }
            references.insert(directive.name, edges);
            specs.insert(
                directive.name,
                Spec {
                    locations: directive.locations.clone(),
                    args: arguments,
                    repeatable: directive.repeatable,
                },
            );
        }
    }

    fn check(
        directives: &[ast::Directive<'_>],
        expected: crate::ast::DirectiveLocation,
        specs: &FxHashMap<&str, Spec>,
        errors: &mut Vec<GraphQLError>,
    ) {
        let mut seen: Vec<&str> = Vec::new();
        for directive in directives {
            let Some(spec) = specs.get(directive.name) else {
                errors.push(
                    GraphQLError::new(format!(
                        "Unknown directive \"@{}\"; define it with `directive @{} ...` first.",
                        directive.name, directive.name
                    ))
                    .at(directive.location),
                );
                continue;
            };
            if !spec.locations.contains(&expected) {
                errors.push(
                    GraphQLError::new(format!(
                        "Directive \"@{}\" may not be used on {}.",
                        directive.name,
                        expected.as_str()
                    ))
                    .at(directive.location),
                );
            }
            if !spec.repeatable && seen.contains(&directive.name) {
                errors.push(
                    GraphQLError::new(format!(
                        "Directive \"@{}\" may only be used once at this location.",
                        directive.name
                    ))
                    .at(directive.location),
                );
            }
            seen.push(directive.name);

            let mut seen_arguments: Vec<&str> = Vec::new();
            for argument in &directive.arguments {
                if seen_arguments.contains(&argument.name) {
                    errors.push(
                        GraphQLError::new(format!(
                            "Directive \"@{}\" was given argument \"{}\" more than once.",
                            directive.name, argument.name
                        ))
                        .at(argument.location),
                    );
                }
                seen_arguments.push(argument.name);

                if !spec.args.iter().any(|(name, _)| name == argument.name) {
                    errors.push(
                        GraphQLError::new(format!(
                            "Unknown argument \"{}\" on directive \"@{}\".",
                            argument.name, directive.name
                        ))
                        .at(argument.location),
                    );
                }
            }
            for (name, required) in &spec.args {
                if *required && directive.argument(name).is_none() {
                    errors.push(
                        GraphQLError::new(format!(
                            "Directive \"@{}\" requires a \"{name}\" argument.",
                            directive.name
                        ))
                        .at(directive.location),
                    );
                }
            }
        }
    }

    fn check_field(
        field: &ast::FieldDefinition<'_>,
        specs: &FxHashMap<&str, Spec>,
        errors: &mut Vec<GraphQLError>,
    ) {
        check(&field.directives, Location::FieldDefinition, specs, errors);
        for argument in &field.arguments {
            check(
                &argument.directives,
                Location::ArgumentDefinition,
                specs,
                errors,
            );
        }
    }

    let mut errors = Vec::new();
    for definition in &document.definitions {
        let Definition::TypeSystem(definition) = definition else {
            continue;
        };
        match definition {
            TypeSystemDefinition::Schema(schema) => {
                check(&schema.directives, Location::Schema, &specs, &mut errors);
            }
            TypeSystemDefinition::SchemaExtension(extension) => {
                check(&extension.directives, Location::Schema, &specs, &mut errors);
            }
            TypeSystemDefinition::Directive(directive) => {
                check(
                    &directive.directives,
                    Location::DirectiveDefinition,
                    &specs,
                    &mut errors,
                );
                // An argument of a directive definition may itself be annotated, and those
                // applications use ARGUMENT_DEFINITION (not DIRECTIVE_DEFINITION).
                for argument in &directive.arguments {
                    check(
                        &argument.directives,
                        Location::ArgumentDefinition,
                        &specs,
                        &mut errors,
                    );
                }
            }
            TypeSystemDefinition::Type(definition) => {
                let location = match &definition.kind {
                    TypeDefinitionKind::Scalar => Location::Scalar,
                    TypeDefinitionKind::Object(_) => Location::Object,
                    TypeDefinitionKind::Interface(_) => Location::Interface,
                    TypeDefinitionKind::Union(_) => Location::Union,
                    TypeDefinitionKind::Enum(_) => Location::Enum,
                    TypeDefinitionKind::InputObject(_) => Location::InputObject,
                };
                check(&definition.directives, location, &specs, &mut errors);
                match &definition.kind {
                    TypeDefinitionKind::Scalar | TypeDefinitionKind::Union(_) => {}
                    TypeDefinitionKind::Object(object) => {
                        for field in &object.fields {
                            check_field(field, &specs, &mut errors);
                        }
                    }
                    TypeDefinitionKind::Interface(interface) => {
                        for field in &interface.fields {
                            check_field(field, &specs, &mut errors);
                        }
                    }
                    TypeDefinitionKind::Enum(enumeration) => {
                        for value in &enumeration.values {
                            check(&value.directives, Location::EnumValue, &specs, &mut errors);
                        }
                    }
                    TypeDefinitionKind::InputObject(input) => {
                        for field in &input.fields {
                            check(
                                &field.directives,
                                Location::InputFieldDefinition,
                                &specs,
                                &mut errors,
                            );
                        }
                    }
                }
            }
            TypeSystemDefinition::TypeExtension(extension) => {
                let location = match &extension.kind {
                    TypeExtensionKind::Scalar => Location::Scalar,
                    TypeExtensionKind::Object(_) => Location::Object,
                    TypeExtensionKind::Interface(_) => Location::Interface,
                    TypeExtensionKind::Union(_) => Location::Union,
                    TypeExtensionKind::Enum(_) => Location::Enum,
                    TypeExtensionKind::InputObject(_) => Location::InputObject,
                };
                check(&extension.directives, location, &specs, &mut errors);
                match &extension.kind {
                    TypeExtensionKind::Scalar | TypeExtensionKind::Union(_) => {}
                    TypeExtensionKind::Object(object) => {
                        for field in &object.fields {
                            check_field(field, &specs, &mut errors);
                        }
                    }
                    TypeExtensionKind::Interface(interface) => {
                        for field in &interface.fields {
                            check_field(field, &specs, &mut errors);
                        }
                    }
                    TypeExtensionKind::Enum(enumeration) => {
                        for value in &enumeration.values {
                            check(&value.directives, Location::EnumValue, &specs, &mut errors);
                        }
                    }
                    TypeExtensionKind::InputObject(input) => {
                        for field in &input.fields {
                            check(
                                &field.directives,
                                Location::InputFieldDefinition,
                                &specs,
                                &mut errors,
                            );
                        }
                    }
                }
            }
        }
    }

    // A schema definition and its extensions are one location. A non-repeatable directive may
    // not be split across them either. Duplicates inside one definition are already reported by
    // the per-node pass above.
    let mut schema_seen: Vec<(&str, usize)> = Vec::new();
    for (index, definition) in document.definitions.iter().enumerate() {
        let directives = match definition {
            Definition::TypeSystem(TypeSystemDefinition::Schema(schema)) => &schema.directives,
            Definition::TypeSystem(TypeSystemDefinition::SchemaExtension(extension)) => {
                &extension.directives
            }
            _ => continue,
        };
        for directive in directives {
            let Some(spec) = specs.get(directive.name) else {
                continue;
            };
            if !spec.repeatable
                && schema_seen
                    .iter()
                    .any(|(name, seen)| *name == directive.name && *seen != index)
            {
                errors.push(
                    GraphQLError::new(format!(
                        "Directive \"@{}\" may only be used once on the schema.",
                        directive.name
                    ))
                    .at(directive.location),
                );
            }
            schema_seen.push((directive.name, index));
        }
    }

    // A directive referenced by one of its own argument definitions creates a cycle in the
    // schema. The specification forbids both direct and indirect self-reference.
    fn reaches(
        current: &str,
        target: &str,
        references: &FxHashMap<&str, Vec<&str>>,
        visited: &mut Vec<String>,
    ) -> bool {
        if visited.iter().any(|name| name == current) {
            return false;
        }
        visited.push(current.to_owned());
        references.get(current).is_some_and(|edges| {
            edges
                .iter()
                .any(|next| *next == target || reaches(next, target, references, visited))
        })
    }

    for (&name, edges) in &references {
        let mut visited = Vec::new();
        if edges
            .iter()
            .any(|next| *next == name || reaches(next, name, &references, &mut visited))
        {
            errors.push(GraphQLError::new(format!(
                "Directive \"@{name}\" cannot reference itself through its argument directives."
            )));
        }
    }

    errors
}

#[derive(Default)]
struct Definitions {
    types: Vec<RawType>,
    directive_definitions: Vec<DirectiveDefinition>,
    query: Option<Name>,
    mutation: Option<Name>,
    subscription: Option<Name>,
    description: Option<Arc<str>>,
    /// Schema-level applications, in source order, validated before the schema is built.
    applied_directives: Vec<AppliedDirective>,
    errors: Vec<GraphQLError>,
    seen: FxHashMap<Name, usize>,
    schema_definition_seen: bool,
    roots_seen: Vec<ast::OperationKind>,
}

enum RawType {
    Scalar {
        description: Option<Arc<str>>,
        name: Name,
        specified_by: Option<Arc<str>>,
        directives: Vec<AppliedDirective>,
    },
    Object {
        description: Option<Arc<str>>,
        name: Name,
        interfaces: Vec<Name>,
        fields: Vec<Field>,
        directives: Vec<AppliedDirective>,
    },
    Interface {
        description: Option<Arc<str>>,
        name: Name,
        interfaces: Vec<Name>,
        fields: Vec<Field>,
        directives: Vec<AppliedDirective>,
    },
    Union {
        description: Option<Arc<str>>,
        name: Name,
        members: Vec<Name>,
        directives: Vec<AppliedDirective>,
    },
    Enum {
        description: Option<Arc<str>>,
        name: Name,
        values: Vec<EnumValue>,
        directives: Vec<AppliedDirective>,
    },
    InputObject {
        description: Option<Arc<str>>,
        name: Name,
        fields: Vec<InputValue>,
        one_of: bool,
        directives: Vec<AppliedDirective>,
    },
}

impl RawType {
    fn name(&self) -> &Name {
        match self {
            Self::Scalar { name, .. }
            | Self::Object { name, .. }
            | Self::Interface { name, .. }
            | Self::Union { name, .. }
            | Self::Enum { name, .. }
            | Self::InputObject { name, .. } => name,
        }
    }
}

impl Definitions {
    fn add(&mut self, definition: &TypeSystemDefinition<'_>) {
        match definition {
            TypeSystemDefinition::Schema(schema) => {
                if self.schema_definition_seen {
                    self.errors.push(GraphQLError::new(
                        "There can be only one schema definition.",
                    ));
                }
                self.schema_definition_seen = true;
                self.description = schema
                    .description
                    .as_ref()
                    .map(|value| Arc::from(value.as_ref()));
                self.applied_directives
                    .extend(parsed_directives(&schema.directives, &mut self.errors));
                for operation in &schema.operations {
                    self.set_root(operation.operation, operation.ty);
                }
            }
            TypeSystemDefinition::SchemaExtension(extension) => {
                self.applied_directives
                    .extend(parsed_directives(&extension.directives, &mut self.errors));
                for operation in &extension.operations {
                    self.set_root(operation.operation, operation.ty);
                }
            }
            TypeSystemDefinition::Directive(directive) => {
                let mut result = DirectiveDefinition::new(directive.name);
                result.description = directive
                    .description
                    .as_ref()
                    .map(|value| Arc::from(value.as_ref()));
                result.repeatable = directive.repeatable;
                result.locations = directive.locations.clone();
                result.directives = parsed_directives(&directive.directives, &mut self.errors);
                result.deprecation_reason = deprecation_reason(&result.directives);
                for argument in &directive.arguments {
                    if let Some(argument) = input_value(argument, &mut self.errors) {
                        result = result.arg(argument);
                    }
                }
                self.directive_definitions.push(result);
            }
            TypeSystemDefinition::Type(definition) => {
                let name: Name = definition.name.into();
                if name.starts_with("__") {
                    self.errors.push(GraphQLError::new(format!(
                        "Type name \"{name}\" must not begin with \"__\", which is reserved by GraphQL introspection."
                    )));
                    return;
                }
                if self.seen.contains_key(&name) {
                    self.errors.push(GraphQLError::new(format!(
                        "There can be only one type named \"{name}\"."
                    )));
                    return;
                }
                let description = definition
                    .description
                    .as_ref()
                    .map(|value| Arc::from(value.as_ref()));
                let directives = parsed_directives(&definition.directives, &mut self.errors);
                let raw = match &definition.kind {
                    TypeDefinitionKind::Scalar => RawType::Scalar {
                        description,
                        name,
                        specified_by: specified_by_url(&directives),
                        directives,
                    },
                    TypeDefinitionKind::Object(object) => RawType::Object {
                        description,
                        name,
                        interfaces: object
                            .interfaces
                            .iter()
                            .map(|name| (*name).into())
                            .collect(),
                        fields: self.fields(&object.fields),
                        directives,
                    },
                    TypeDefinitionKind::Interface(interface) => RawType::Interface {
                        description,
                        name,
                        interfaces: interface
                            .interfaces
                            .iter()
                            .map(|name| (*name).into())
                            .collect(),
                        fields: self.fields(&interface.fields),
                        directives,
                    },
                    TypeDefinitionKind::Union(union) => RawType::Union {
                        description,
                        name,
                        members: union.types.iter().map(|name| (*name).into()).collect(),
                        directives,
                    },
                    TypeDefinitionKind::Enum(enumeration) => RawType::Enum {
                        description,
                        name,
                        values: enumeration
                            .values
                            .iter()
                            .map(|value| enum_value(value, &mut self.errors))
                            .collect(),
                        directives,
                    },
                    TypeDefinitionKind::InputObject(input) => RawType::InputObject {
                        description,
                        name,
                        fields: input
                            .fields
                            .iter()
                            .filter_map(|field| input_value(field, &mut self.errors))
                            .collect(),
                        one_of: has_directive(&directives, "oneOf"),
                        directives,
                    },
                };
                self.seen.insert(raw.name().clone(), self.types.len());
                self.types.push(raw);
            }
            TypeSystemDefinition::TypeExtension(extension) => {
                let name: Name = extension.name.into();
                let Some(index) = self.seen.get(&name).copied() else {
                    self.errors.push(GraphQLError::new(format!(
                        "Cannot extend unknown type \"{name}\"."
                    )));
                    return;
                };
                let directives = parsed_directives(&extension.directives, &mut self.errors);
                let mut extra_fields = match &extension.kind {
                    TypeExtensionKind::Object(data) => Some(self.fields(&data.fields)),
                    TypeExtensionKind::Interface(data) => Some(self.fields(&data.fields)),
                    _ => None,
                };
                match (&mut self.types[index], &extension.kind) {
                    (
                        RawType::Object {
                            interfaces,
                            fields,
                            directives: type_directives,
                            ..
                        },
                        TypeExtensionKind::Object(data),
                    ) => {
                        interfaces.extend(data.interfaces.iter().map(|name| (*name).into()));
                        fields.extend(extra_fields.take().unwrap_or_default());
                        type_directives.extend(directives);
                    }
                    (
                        RawType::Interface {
                            interfaces,
                            fields,
                            directives: type_directives,
                            ..
                        },
                        TypeExtensionKind::Interface(data),
                    ) => {
                        interfaces.extend(data.interfaces.iter().map(|name| (*name).into()));
                        fields.extend(extra_fields.take().unwrap_or_default());
                        type_directives.extend(directives);
                    }
                    (
                        RawType::Union {
                            members,
                            directives: type_directives,
                            ..
                        },
                        TypeExtensionKind::Union(data),
                    ) => {
                        members.extend(data.types.iter().map(|name| (*name).into()));
                        type_directives.extend(directives);
                    }
                    (
                        RawType::Enum {
                            values,
                            directives: type_directives,
                            ..
                        },
                        TypeExtensionKind::Enum(data),
                    ) => {
                        values.extend(
                            data.values
                                .iter()
                                .map(|value| enum_value(value, &mut self.errors)),
                        );
                        type_directives.extend(directives);
                    }
                    (
                        RawType::InputObject {
                            fields,
                            one_of,
                            directives: type_directives,
                            ..
                        },
                        TypeExtensionKind::InputObject(data),
                    ) => {
                        if has_directive(&directives, "oneOf") {
                            *one_of = true;
                        }
                        fields.extend(
                            data.fields
                                .iter()
                                .filter_map(|field| input_value(field, &mut self.errors)),
                        );
                        type_directives.extend(directives);
                    }
                    (
                        RawType::Scalar {
                            specified_by,
                            directives: type_directives,
                            ..
                        },
                        TypeExtensionKind::Scalar,
                    ) => {
                        if let Some(url) = specified_by_url(&directives) {
                            *specified_by = Some(url);
                        }
                        type_directives.extend(directives);
                    }
                    (raw, _) => {
                        self.errors.push(GraphQLError::new(format!(
                            "Type \"{name}\" was extended with the wrong kind (it is a {}).",
                            match raw {
                                RawType::Scalar { .. } => "scalar",
                                RawType::Object { .. } => "object",
                                RawType::Interface { .. } => "interface",
                                RawType::Union { .. } => "union",
                                RawType::Enum { .. } => "enum",
                                RawType::InputObject { .. } => "input object",
                            }
                        )));
                    }
                }
            }
        }
    }

    fn set_root(&mut self, kind: ast::OperationKind, name: &str) {
        if self.roots_seen.contains(&kind) {
            self.errors.push(GraphQLError::new(format!(
                "The {kind} root type is defined more than once."
            )));
        } else {
            self.roots_seen.push(kind);
        }
        match kind {
            ast::OperationKind::Query => self.query = Some(name.into()),
            ast::OperationKind::Mutation => self.mutation = Some(name.into()),
            ast::OperationKind::Subscription => self.subscription = Some(name.into()),
        }
    }

    fn fields(&mut self, fields: &[ast::FieldDefinition<'_>]) -> Vec<Field> {
        fields
            .iter()
            .map(|field| {
                let mut result = Field::new(field.name, type_ref(&field.ty));
                result.description = field
                    .description
                    .as_ref()
                    .map(|value| Arc::from(value.as_ref()));
                for argument in &field.arguments {
                    if let Some(argument) = input_value(argument, &mut self.errors) {
                        result = result.arg(argument);
                    }
                }
                result.directives = parsed_directives(&field.directives, &mut self.errors);
                result.deprecation_reason = deprecation_reason(&result.directives);
                result
            })
            .collect()
    }

    fn finish(self) -> Result<Schema, SchemaError> {
        let mut errors = self.errors;
        // The schema definition and its extensions form one directive location. A non-repeatable
        // directive may not be split across them. (The AST pass already rejects duplicates within
        // one definition; this covers the merged document.)
        let mut seen: Vec<Name> = Vec::new();
        for directive in &self.applied_directives {
            let repeatable = self
                .directive_definitions
                .iter()
                .find(|definition| definition.name == directive.name)
                .is_some_and(|definition| definition.repeatable);
            if !repeatable && seen.contains(&directive.name) {
                errors.push(GraphQLError::new(format!(
                    "Directive \"@{}\" may only be used once on the schema.",
                    directive.name
                )));
            }
            seen.push(directive.name.clone());
        }
        if !errors.is_empty() {
            return Err(SchemaError { errors });
        }

        let mut builder = SchemaBuilder::new();
        if let Some(description) = self.description {
            builder = builder.description(description);
        }
        builder = builder.schema_directives(self.applied_directives);
        for raw in self.types {
            let ty = match raw {
                RawType::Scalar {
                    description,
                    name,
                    specified_by,
                    directives,
                } => {
                    let mut scalar = ScalarType::new(name);
                    scalar.description = description;
                    scalar.specified_by_url = specified_by;
                    scalar.directives = directives;
                    Type::Scalar(Arc::new(scalar))
                }
                RawType::Object {
                    description,
                    name,
                    interfaces,
                    fields,
                    directives,
                } => {
                    let mut object = ObjectType::new(name);
                    object.description = description;
                    object.interfaces = interfaces;
                    object.directives = directives;
                    for field in fields {
                        object.insert_field(field);
                    }
                    Type::Object(Arc::new(object))
                }
                RawType::Interface {
                    description,
                    name,
                    interfaces,
                    fields,
                    directives,
                } => {
                    let mut interface = InterfaceType::new(name);
                    interface.description = description;
                    interface.interfaces = interfaces;
                    interface.directives = directives;
                    for field in fields {
                        interface.insert_field(field);
                    }
                    Type::Interface(Arc::new(interface))
                }
                RawType::Union {
                    description,
                    name,
                    members,
                    directives,
                } => {
                    let mut union = UnionType::new(name);
                    union.description = description;
                    union.types = members;
                    union.directives = directives;
                    Type::Union(Arc::new(union))
                }
                RawType::Enum {
                    description,
                    name,
                    values,
                    directives,
                } => {
                    let mut enumeration = EnumType::new(name);
                    enumeration.description = description;
                    enumeration.directives = directives;
                    for value in values {
                        if enumeration.values.contains_key(&value.name) {
                            enumeration.duplicates.push(value.name.clone());
                        }
                        enumeration.values.insert(value.name.clone(), value);
                    }
                    Type::Enum(Arc::new(enumeration))
                }
                RawType::InputObject {
                    description,
                    name,
                    fields,
                    one_of,
                    directives,
                } => {
                    let mut input = InputObjectType::new(name);
                    input.description = description;
                    input.one_of = one_of;
                    input.directives = directives;
                    for field in fields {
                        input.insert_field(field);
                    }
                    Type::InputObject(Arc::new(input))
                }
            };
            builder = builder.register(ty);
        }
        for directive in self.directive_definitions {
            builder = builder.directive(directive);
        }
        // The type system language allows the default root names to be omitted: `Query`,
        // `Mutation` and `Subscription` are inferred when they exist. That inference only applies
        // to a document with no schema definition: an explicit `schema { ... }` block is the
        // complete statement of which roots exist, so a default-named type in the document is an
        // ordinary type unless the block names it as a root.
        let (query, mutation, subscription) = if self.schema_definition_seen {
            if self.query.is_none() {
                errors.push(GraphQLError::new(
                    "A schema definition must define a query root type.",
                ));
            }
            (self.query, self.mutation, self.subscription)
        } else {
            let query = self
                .query
                .or_else(|| builder.has_type("Query").then(|| "Query".into()));
            let mutation = self
                .mutation
                .or_else(|| builder.has_type("Mutation").then(|| "Mutation".into()));
            let subscription = self.subscription.or_else(|| {
                builder
                    .has_type("Subscription")
                    .then(|| "Subscription".into())
            });
            (query, mutation, subscription)
        };
        if !errors.is_empty() {
            return Err(SchemaError { errors });
        }
        if let Some(query) = query {
            builder = builder.query(query);
        }
        if let Some(mutation) = mutation {
            builder = builder.mutation(mutation);
        }
        if let Some(subscription) = subscription {
            builder = builder.subscription(subscription);
        }
        builder.build()
    }
}

fn type_ref(ty: &ast::Type<'_>) -> TypeRef {
    match ty {
        // Built-in type names are interned, so a reference to `String` or `Int` shares one
        // allocation instead of making a new `Arc<str>` for every field and argument.
        ast::Type::Named(name) => TypeRef::named_str(name),
        ast::Type::List(inner) => type_ref(inner).list(),
        ast::Type::NonNull(inner) => type_ref(inner).non_null(),
    }
}

/// Convert applied directives, preserving source order and argument order.
fn parsed_directives(
    directives: &[ast::Directive<'_>],
    errors: &mut Vec<GraphQLError>,
) -> Vec<AppliedDirective> {
    directives
        .iter()
        .map(|directive| {
            let mut result = AppliedDirective::new(directive.name);
            for argument in &directive.arguments {
                match const_value(&argument.value) {
                    Ok(value) => {
                        result.arguments.insert(argument.name.into(), value);
                    }
                    Err(error) => errors.push(error.at(argument.location)),
                }
            }
            result
        })
        .collect()
}

/// The specification's default `@deprecated` reason.
const DEFAULT_DEPRECATION_REASON: &str = "No longer supported";

/// Read the authoritative deprecation reason from an applied `@deprecated`.
///
/// Only a `String` argument is used. Validation rejects a wrong-typed reason before a schema is
/// returned, so a bogus value never becomes a deprecation reason. An omitted or explicit `null`
/// reason uses the specification's default.
fn deprecation_reason(directives: &[AppliedDirective]) -> Option<Arc<str>> {
    let directive = directives
        .iter()
        .rev()
        .find(|directive| directive.name.as_ref() == "deprecated")?;
    match directive.get_argument("reason") {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Null) | None => Some(Arc::from(DEFAULT_DEPRECATION_REASON)),
        Some(_) => None,
    }
}

/// Read the authoritative URL from an applied `@specifiedBy`.
fn specified_by_url(directives: &[AppliedDirective]) -> Option<Arc<str>> {
    let directive = directives
        .iter()
        .rev()
        .find(|directive| directive.name.as_ref() == "specifiedBy")?;
    match directive.get_argument("url") {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn has_directive(directives: &[AppliedDirective], name: &str) -> bool {
    directives
        .iter()
        .any(|directive| directive.name.as_ref() == name)
}

fn enum_value(value: &ast::EnumValueDefinition<'_>, errors: &mut Vec<GraphQLError>) -> EnumValue {
    let directives = parsed_directives(&value.directives, errors);
    EnumValue {
        name: value.name.into(),
        description: value.description.as_ref().map(|v| Arc::from(v.as_ref())),
        deprecation_reason: deprecation_reason(&directives),
        directives,
    }
}

fn input_value(
    value: &ast::InputValueDefinition<'_>,
    errors: &mut Vec<GraphQLError>,
) -> Option<InputValue> {
    let mut result = InputValue::new(value.name, type_ref(&value.ty));
    result.description = value
        .description
        .as_ref()
        .map(|value| Arc::from(value.as_ref()));
    result.directives = parsed_directives(&value.directives, errors);
    result.deprecation_reason = deprecation_reason(&result.directives);
    if let Some(default) = &value.default_value {
        match const_value(default) {
            Ok(value) => result.default_value = Some(value),
            Err(error) => {
                errors.push(error.at(value.location));
                return None;
            }
        }
    }
    Some(result)
}

fn const_value(value: &ast::Value<'_>) -> Result<Value, GraphQLError> {
    Ok(match value {
        ast::Value::Variable(name) => {
            return Err(GraphQLError::new(format!(
                "Variable \"${name}\" is not allowed in a constant value."
            )));
        }
        ast::Value::Int(value) => Value::integer(*value),
        ast::Value::Float(value) => Value::Float(*value),
        ast::Value::String(value) => Value::String(Arc::from(value.as_ref())),
        ast::Value::Boolean(value) => Value::Boolean(*value),
        ast::Value::Null => Value::Null,
        ast::Value::Enum(name) => Value::Enum((*name).into()),
        ast::Value::List(values) => Value::List(
            values
                .iter()
                .map(const_value)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        ast::Value::Object(fields) => {
            let mut object = crate::Object::with_capacity(fields.len());
            for field in fields {
                object.insert(field.name, const_value(&field.value)?);
            }
            Value::Object(Arc::new(object))
        }
    })
}

/// Validate every directive argument literal against the schema's directive definitions.
///
/// This runs after the schema is built so custom scalars, enums and input objects coerce exactly
/// as they do for field arguments. Built-in directives are included, which means a wrong-typed
/// `@deprecated(reason: ...)` or `@specifiedBy(url: ...)` is rejected instead of silently
/// becoming `""`.
/// `argument "name" of <owner>`, formatted only when an error is recorded.
struct LowerArgument<'a> {
    name: &'a str,
    owner: &'a dyn core::fmt::Display,
}

impl core::fmt::Display for LowerArgument<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "argument \"{}\" of {}", self.name, self.owner)
    }
}

/// Validate every directive applied at one location against the built schema.
fn check_directive_values(
    schema: &Schema,
    directives: &[ast::Directive<'_>],
    owner: &dyn core::fmt::Display,
    errors: &mut Vec<GraphQLError>,
) {
    for directive in directives {
        let Some(definition) = schema.get_directive(directive.name) else {
            continue;
        };
        for argument in &directive.arguments {
            let Some(declared) = definition.args.get(argument.name) else {
                continue;
            };
            if let Err(error) = crate::execution::coerce::coerce_literal(
                schema,
                &argument.value,
                &declared.ty,
                None,
            ) {
                errors.push(
                    GraphQLError::new(format!(
                        "Argument \"{}\" of directive \"@{}\" on {owner} is invalid: {}",
                        argument.name,
                        directive.name,
                        error.message()
                    ))
                    .at(argument.location),
                );
            }
        }
    }
}

fn validate_directive_values(schema: &Schema, document: &ast::Document<'_>) -> Vec<GraphQLError> {
    let mut errors = Vec::new();
    for definition in &document.definitions {
        let Definition::TypeSystem(definition) = definition else {
            continue;
        };
        match definition {
            TypeSystemDefinition::Schema(schema_definition) => {
                check_directive_values(
                    schema,
                    &schema_definition.directives,
                    &"the schema",
                    &mut errors,
                );
            }
            TypeSystemDefinition::SchemaExtension(extension) => {
                check_directive_values(schema, &extension.directives, &"the schema", &mut errors);
            }
            TypeSystemDefinition::Directive(directive) => {
                check_directive_values(
                    schema,
                    &directive.directives,
                    &DirectiveOwner(directive.name),
                    &mut errors,
                );
                for argument in &directive.arguments {
                    let owner = LowerArgument {
                        name: argument.name,
                        owner: &DirectiveOwner(directive.name),
                    };
                    check_directive_values(schema, &argument.directives, &owner, &mut errors);
                }
            }
            TypeSystemDefinition::Type(definition) => {
                let owner = NamedOwner {
                    kind: "type",
                    name: definition.name,
                };
                check_directive_values(schema, &definition.directives, &owner, &mut errors);
                match &definition.kind {
                    TypeDefinitionKind::Scalar | TypeDefinitionKind::Union(_) => {}
                    TypeDefinitionKind::Object(object) => {
                        check_fields(schema, definition.name, &object.fields, &mut errors);
                    }
                    TypeDefinitionKind::Interface(interface) => {
                        check_fields(schema, definition.name, &interface.fields, &mut errors);
                    }
                    TypeDefinitionKind::Enum(enumeration) => {
                        for value in &enumeration.values {
                            let owner = NamedPath {
                                kind: "enum value",
                                type_name: definition.name,
                                name: value.name,
                            };
                            check_directive_values(schema, &value.directives, &owner, &mut errors);
                        }
                    }
                    TypeDefinitionKind::InputObject(input) => {
                        for field in &input.fields {
                            let owner = NamedPath {
                                kind: "input field",
                                type_name: definition.name,
                                name: field.name,
                            };
                            check_directive_values(schema, &field.directives, &owner, &mut errors);
                        }
                    }
                }
            }
            TypeSystemDefinition::TypeExtension(extension) => {
                let owner = NamedOwner {
                    kind: "type",
                    name: extension.name,
                };
                check_directive_values(schema, &extension.directives, &owner, &mut errors);
                match &extension.kind {
                    TypeExtensionKind::Scalar | TypeExtensionKind::Union(_) => {}
                    TypeExtensionKind::Object(object) => {
                        check_fields(schema, extension.name, &object.fields, &mut errors);
                    }
                    TypeExtensionKind::Interface(interface) => {
                        check_fields(schema, extension.name, &interface.fields, &mut errors);
                    }
                    TypeExtensionKind::Enum(enumeration) => {
                        for value in &enumeration.values {
                            let owner = NamedPath {
                                kind: "enum value",
                                type_name: extension.name,
                                name: value.name,
                            };
                            check_directive_values(schema, &value.directives, &owner, &mut errors);
                        }
                    }
                    TypeExtensionKind::InputObject(input) => {
                        for field in &input.fields {
                            let owner = NamedPath {
                                kind: "input field",
                                type_name: extension.name,
                                name: field.name,
                            };
                            check_directive_values(schema, &field.directives, &owner, &mut errors);
                        }
                    }
                }
            }
        }
    }
    errors
}

fn check_fields(
    schema: &Schema,
    type_name: &str,
    fields: &[ast::FieldDefinition<'_>],
    errors: &mut Vec<GraphQLError>,
) {
    for field in fields {
        let owner = NamedPath {
            kind: "field",
            type_name,
            name: field.name,
        };
        check_directive_values(schema, &field.directives, &owner, errors);
        for argument in &field.arguments {
            let owner = LowerArgument {
                name: argument.name,
                owner: &NamedPath {
                    kind: "field",
                    type_name,
                    name: field.name,
                },
            };
            check_directive_values(schema, &argument.directives, &owner, errors);
        }
    }
}

/// Reject a non-repeatable directive that was applied once on a definition and once on one of
/// its extensions (or on two extensions).
///
/// Within a single definition the AST checker catches duplicates; extensions are merged into the
/// model later, so this pass is what makes `scalar X @specifiedBy(...)` plus
/// `extend scalar X @specifiedBy(...)` fail.
fn check_model_directives(schema: &Schema, errors: &mut Vec<GraphQLError>) {
    fn check(
        schema: &Schema,
        directives: &[AppliedDirective],
        owner: &dyn core::fmt::Display,
        errors: &mut Vec<GraphQLError>,
    ) {
        let mut seen: Vec<&str> = Vec::new();
        for directive in directives {
            if schema
                .get_directive(&directive.name)
                .is_some_and(|definition| definition.repeatable)
            {
                continue;
            }
            if seen.contains(&directive.name.as_ref()) {
                errors.push(GraphQLError::new(format!(
                    "Directive \"@{}\" may only be used once on {owner}.",
                    directive.name
                )));
            }
            seen.push(&directive.name);
        }
    }

    for (_, ty) in schema.types() {
        match ty {
            Type::Scalar(scalar) => {
                check(
                    schema,
                    &scalar.directives,
                    &NamedOwner {
                        kind: "scalar",
                        name: &scalar.name,
                    },
                    errors,
                );
            }
            Type::Object(object) => {
                check(
                    schema,
                    &object.directives,
                    &NamedOwner {
                        kind: "object",
                        name: &object.name,
                    },
                    errors,
                );
                for field in object.fields.values() {
                    let owner = NamedPath {
                        kind: "field",
                        type_name: &object.name,
                        name: &field.name,
                    };
                    check(schema, &field.directives, &owner, errors);
                    for argument in field.args.values() {
                        check(
                            schema,
                            &argument.directives,
                            &LowerArgument {
                                name: &argument.name,
                                owner: &owner,
                            },
                            errors,
                        );
                    }
                }
            }
            Type::Interface(interface) => {
                check(
                    schema,
                    &interface.directives,
                    &NamedOwner {
                        kind: "interface",
                        name: &interface.name,
                    },
                    errors,
                );
                for field in interface.fields.values() {
                    let owner = NamedPath {
                        kind: "field",
                        type_name: &interface.name,
                        name: &field.name,
                    };
                    check(schema, &field.directives, &owner, errors);
                    for argument in field.args.values() {
                        check(
                            schema,
                            &argument.directives,
                            &LowerArgument {
                                name: &argument.name,
                                owner: &owner,
                            },
                            errors,
                        );
                    }
                }
            }
            Type::Union(union) => {
                check(
                    schema,
                    &union.directives,
                    &NamedOwner {
                        kind: "union",
                        name: &union.name,
                    },
                    errors,
                );
            }
            Type::Enum(enumeration) => {
                check(
                    schema,
                    &enumeration.directives,
                    &NamedOwner {
                        kind: "enum",
                        name: &enumeration.name,
                    },
                    errors,
                );
                for value in enumeration.values.values() {
                    let owner = NamedPath {
                        kind: "enum value",
                        type_name: &enumeration.name,
                        name: &value.name,
                    };
                    check(schema, &value.directives, &owner, errors);
                }
            }
            Type::InputObject(input) => {
                check(
                    schema,
                    &input.directives,
                    &NamedOwner {
                        kind: "input object",
                        name: &input.name,
                    },
                    errors,
                );
                for field in input.fields.values() {
                    let owner = NamedPath {
                        kind: "input field",
                        type_name: &input.name,
                        name: &field.name,
                    };
                    check(schema, &field.directives, &owner, errors);
                }
            }
        }
    }

    for (_, directive) in schema.directives() {
        check(
            schema,
            &directive.directives,
            &DirectiveOwner(&directive.name),
            errors,
        );
        for argument in directive.args.values() {
            check(
                schema,
                &argument.directives,
                &LowerArgument {
                    name: &argument.name,
                    owner: &DirectiveOwner(&directive.name),
                },
                errors,
            );
        }
    }
}

/// Validate a list of applied directives against a built schema.
///
/// SDL parsing also validates through the AST so it can report source locations; this helper
/// covers code-first builders, which have no AST, and the built-schema pass that catches duplicate
/// applications split across SDL extensions. It checks that the directive exists, that it is
/// allowed at `location`, that every argument is declared and has the right type, that required
/// arguments are present, and that a non-repeatable directive is not listed twice.
///
/// `SchemaBuilder::build` calls this after all definitions are registered. The caller owns
/// `errors`, so several elements can be validated into one build error.
pub(crate) fn validate_applied_directives(
    schema: &Schema,
    directives: &[AppliedDirective],
    location: ast::DirectiveLocation,
    owner: &dyn core::fmt::Display,
    errors: &mut SchemaError,
) {
    let mut seen: Vec<&str> = Vec::new();
    for directive in directives {
        let Some(definition) = schema.get_directive(&directive.name) else {
            errors.push(format!(
                "Unknown directive \"@{}\" on {owner}.",
                directive.name
            ));
            continue;
        };
        if !definition.locations.contains(&location) {
            errors.push(format!(
                "Directive \"@{}\" may not be used on {}.",
                directive.name,
                location.as_str()
            ));
        }
        if !definition.repeatable && seen.contains(&directive.name.as_ref()) {
            errors.push(format!(
                "Directive \"@{}\" may only be used once on {owner}.",
                directive.name
            ));
        }
        seen.push(&directive.name);

        for (name, value) in &directive.arguments {
            let Some(declared) = definition.args.get(name.as_ref()) else {
                errors.push(format!(
                    "Unknown argument \"{name}\" on directive \"@{}\" applied to {owner}.",
                    directive.name
                ));
                continue;
            };
            if let Err(error) =
                crate::execution::coerce::validate_default_value(schema, value, &declared.ty)
            {
                errors.push(format!(
                    "Argument \"{name}\" of directive \"@{}\" on {owner} is invalid: {}",
                    directive.name,
                    error.message()
                ));
            }
        }
        for argument in definition.args.values() {
            if matches!(argument.ty, TypeRef::NonNull(_))
                && argument.default_value.is_none()
                && directive.get_argument(&argument.name).is_none()
            {
                errors.push(format!(
                    "Directive \"@{}\" requires a \"{}\" argument on {owner}.",
                    directive.name, argument.name
                ));
            }
        }
    }
}
