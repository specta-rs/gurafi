//! Build a [`Schema`] from a schema definition language document.
//!
//! SDL-described schemas have no resolvers; they are useful for tooling, compatibility checks,
//! introspection snapshots and code generation. Server code normally builds schemas with the
//! builder (or with `gurafi`) and attaches resolvers.

use std::{collections::HashMap, sync::Arc};

use crate::{
    Value,
    ast::{self, Definition, TypeDefinitionKind, TypeExtensionKind, TypeSystemDefinition},
    error::GraphQLError,
    schema::{Schema, SchemaBuilder, SchemaError},
    types::{
        DirectiveDefinition, EnumType, Field, InputObjectType, InputValue, InterfaceType, Name,
        ObjectType, ScalarType, Type, TypeRef, UnionType,
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
    definitions.finish()
}

/// The directives the specification defines. User directives are added from the document.
fn check_sdl_directives(document: &ast::Document<'_>) -> Vec<GraphQLError> {
    use std::collections::HashMap;

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
    let mut specs: HashMap<&str, Spec> = HashMap::new();
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

    for definition in &document.definitions {
        if let Definition::TypeSystem(TypeSystemDefinition::Directive(directive)) = definition {
            specs.insert(
                directive.name,
                Spec {
                    locations: directive.locations.clone(),
                    args: directive
                        .arguments
                        .iter()
                        .map(|argument| {
                            (
                                argument.name.to_owned(),
                                matches!(argument.ty, ast::Type::NonNull(_))
                                    && argument.default_value.is_none(),
                            )
                        })
                        .collect(),
                    repeatable: directive.repeatable,
                },
            );
        }
    }

    fn check(
        directives: &[ast::Directive<'_>],
        expected: crate::ast::DirectiveLocation,
        specs: &HashMap<&str, Spec>,
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
            for argument in &directive.arguments {
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
        specs: &HashMap<&str, Spec>,
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
            TypeSystemDefinition::Directive(_) => {}
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
    errors: Vec<GraphQLError>,
    seen: HashMap<Name, usize>,
    schema_definition_seen: bool,
    roots_seen: Vec<ast::OperationKind>,
}

enum RawType {
    Scalar {
        description: Option<Arc<str>>,
        name: Name,
        specified_by: Option<Arc<str>>,
    },
    Object {
        description: Option<Arc<str>>,
        name: Name,
        interfaces: Vec<Name>,
        fields: Vec<Field>,
    },
    Interface {
        description: Option<Arc<str>>,
        name: Name,
        interfaces: Vec<Name>,
        fields: Vec<Field>,
    },
    Union {
        description: Option<Arc<str>>,
        name: Name,
        members: Vec<Name>,
    },
    Enum {
        description: Option<Arc<str>>,
        name: Name,
        values: Vec<(Name, Option<Arc<str>>, Option<Arc<str>>)>,
    },
    InputObject {
        description: Option<Arc<str>>,
        name: Name,
        fields: Vec<InputValue>,
        one_of: bool,
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
                for operation in &schema.operations {
                    self.set_root(operation.operation, operation.ty);
                }
            }
            TypeSystemDefinition::SchemaExtension(extension) => {
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
                for argument in &directive.arguments {
                    match input_value(argument) {
                        Ok(argument) => {
                            result = result.arg(argument);
                        }
                        Err(error) => self.errors.push(error),
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
                if let Some(index) = self.seen.get(&name).copied() {
                    self.errors.push(GraphQLError::new(format!(
                        "There can be only one type named \"{name}\"."
                    )));
                    let _ = index;
                    return;
                }
                let description = definition
                    .description
                    .as_ref()
                    .map(|value| Arc::from(value.as_ref()));
                let raw = match &definition.kind {
                    TypeDefinitionKind::Scalar => RawType::Scalar {
                        description,
                        name,
                        specified_by: directive_string(
                            &definition.directives,
                            "specifiedBy",
                            "url",
                        ),
                    },
                    TypeDefinitionKind::Object(object) => RawType::Object {
                        description,
                        name,
                        interfaces: object
                            .interfaces
                            .iter()
                            .map(|name| (*name).into())
                            .collect(),
                        fields: self.fields("object", &object.fields),
                    },
                    TypeDefinitionKind::Interface(interface) => RawType::Interface {
                        description,
                        name,
                        interfaces: interface
                            .interfaces
                            .iter()
                            .map(|name| (*name).into())
                            .collect(),
                        fields: self.fields("interface", &interface.fields),
                    },
                    TypeDefinitionKind::Union(union) => RawType::Union {
                        description,
                        name,
                        members: union.types.iter().map(|name| (*name).into()).collect(),
                    },
                    TypeDefinitionKind::Enum(enumeration) => RawType::Enum {
                        description,
                        name,
                        values: enumeration
                            .values
                            .iter()
                            .map(|value| {
                                (
                                    value.name.into(),
                                    value.description.as_ref().map(|v| Arc::from(v.as_ref())),
                                    directive_string(&value.directives, "deprecated", "reason"),
                                )
                            })
                            .collect(),
                    },
                    TypeDefinitionKind::InputObject(input) => RawType::InputObject {
                        description,
                        name,
                        fields: input
                            .fields
                            .iter()
                            .filter_map(|field| match input_value(field) {
                                Ok(field) => Some(field),
                                Err(error) => {
                                    self.errors.push(error);
                                    None
                                }
                            })
                            .collect(),
                        one_of: definition
                            .directives
                            .iter()
                            .any(|directive| directive.name == "oneOf"),
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
                let mut extra_fields = match &extension.kind {
                    TypeExtensionKind::Object(data) => Some(self.fields("object", &data.fields)),
                    TypeExtensionKind::Interface(data) => {
                        Some(self.fields("interface", &data.fields))
                    }
                    _ => None,
                };
                match (&mut self.types[index], &extension.kind) {
                    (
                        RawType::Object {
                            interfaces, fields, ..
                        },
                        TypeExtensionKind::Object(data),
                    ) => {
                        interfaces.extend(data.interfaces.iter().map(|name| (*name).into()));
                        fields.extend(extra_fields.take().unwrap_or_default());
                    }
                    (
                        RawType::Interface {
                            interfaces, fields, ..
                        },
                        TypeExtensionKind::Interface(data),
                    ) => {
                        interfaces.extend(data.interfaces.iter().map(|name| (*name).into()));
                        fields.extend(extra_fields.take().unwrap_or_default());
                    }
                    (RawType::Union { members, .. }, TypeExtensionKind::Union(data)) => {
                        members.extend(data.types.iter().map(|name| (*name).into()));
                    }
                    (RawType::Enum { values, .. }, TypeExtensionKind::Enum(data)) => {
                        values.extend(data.values.iter().map(|value| {
                            (
                                value.name.into(),
                                value.description.as_ref().map(|v| Arc::from(v.as_ref())),
                                directive_string(&value.directives, "deprecated", "reason"),
                            )
                        }));
                    }
                    (RawType::InputObject { fields, .. }, TypeExtensionKind::InputObject(data)) => {
                        fields.extend(data.fields.iter().filter_map(|field| {
                            match input_value(field) {
                                Ok(field) => Some(field),
                                Err(error) => {
                                    self.errors.push(error);
                                    None
                                }
                            }
                        }));
                    }
                    (RawType::Scalar { .. }, TypeExtensionKind::Scalar) => {}
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

    fn fields(&mut self, owner: &str, fields: &[ast::FieldDefinition<'_>]) -> Vec<Field> {
        fields
            .iter()
            .map(|field| {
                let mut result = Field::new(field.name, type_ref(&field.ty));
                result.description = field
                    .description
                    .as_ref()
                    .map(|value| Arc::from(value.as_ref()));
                for argument in &field.arguments {
                    match input_value(argument) {
                        Ok(argument) => {
                            result = result.arg(argument);
                        }
                        Err(error) => self.errors.push(error),
                    }
                }
                result.deprecation_reason =
                    directive_string(&field.directives, "deprecated", "reason");
                let _ = owner;
                result
            })
            .collect()
    }

    fn finish(self) -> Result<Schema, SchemaError> {
        if !self.errors.is_empty() {
            return Err(SchemaError {
                errors: self.errors,
            });
        }

        let mut builder = SchemaBuilder::new();
        if let Some(description) = self.description {
            builder = builder.description(description);
        }
        for raw in self.types {
            let ty = match raw {
                RawType::Scalar {
                    description,
                    name,
                    specified_by,
                } => {
                    let mut scalar = ScalarType::new(name);
                    scalar.description = description;
                    scalar.specified_by_url = specified_by;
                    Type::Scalar(Arc::new(scalar))
                }
                RawType::Object {
                    description,
                    name,
                    interfaces,
                    fields,
                } => {
                    let mut object = ObjectType::new(name);
                    object.description = description;
                    object.interfaces = interfaces;
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
                } => {
                    let mut interface = InterfaceType::new(name);
                    interface.description = description;
                    interface.interfaces = interfaces;
                    for field in fields {
                        interface.insert_field(field);
                    }
                    Type::Interface(Arc::new(interface))
                }
                RawType::Union {
                    description,
                    name,
                    members,
                } => {
                    let mut union = UnionType::new(name);
                    union.description = description;
                    union.types = members;
                    Type::Union(Arc::new(union))
                }
                RawType::Enum {
                    description,
                    name,
                    values,
                } => {
                    let mut enumeration = EnumType::new(name);
                    enumeration.description = description;
                    for (value, description, deprecation_reason) in values {
                        if enumeration.values.contains_key(&value) {
                            enumeration.duplicates.push(value.clone());
                        }
                        enumeration.values.insert(
                            value.clone(),
                            crate::types::EnumValue {
                                name: value,
                                description,
                                deprecation_reason,
                            },
                        );
                    }
                    Type::Enum(Arc::new(enumeration))
                }
                RawType::InputObject {
                    description,
                    name,
                    fields,
                    one_of,
                } => {
                    let mut input = InputObjectType::new(name);
                    input.description = description;
                    input.one_of = one_of;
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
        // `Mutation` and `Subscription` are inferred when they exist.
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
        ast::Type::Named(name) => TypeRef::named(*name),
        ast::Type::List(inner) => type_ref(inner).list(),
        ast::Type::NonNull(inner) => type_ref(inner).non_null(),
    }
}

fn input_value(value: &ast::InputValueDefinition<'_>) -> Result<InputValue, GraphQLError> {
    let mut result = InputValue::new(value.name, type_ref(&value.ty));
    result.description = value
        .description
        .as_ref()
        .map(|value| Arc::from(value.as_ref()));
    result.deprecation_reason = directive_string(&value.directives, "deprecated", "reason");
    if let Some(default) = &value.default_value {
        result.default_value = Some(const_value(default)?);
    }
    Ok(result)
}

fn directive_string(
    directives: &[ast::Directive<'_>],
    name: &str,
    argument: &str,
) -> Option<Arc<str>> {
    let directive = directives.iter().find(|directive| directive.name == name)?;
    let value = directive
        .arguments
        .iter()
        .find(|item| item.name == argument)
        .map(|item| &item.value);
    match value {
        Some(ast::Value::String(value)) => Some(Arc::from(value.as_ref())),
        _ if name == "deprecated" => Some(Arc::from("No longer supported")),
        _ => Some(Arc::from("")),
    }
}

fn const_value(value: &ast::Value<'_>) -> Result<Value, GraphQLError> {
    Ok(match value {
        ast::Value::Variable(name) => {
            return Err(GraphQLError::new(format!(
                "Variable \"${name}\" is not allowed in a constant value."
            )));
        }
        ast::Value::Int(value) => {
            let value = i32::try_from(*value).map_err(|_| {
                GraphQLError::new(format!(
                    "Integer \"{value}\" is outside the 32-bit range of the Int scalar; use a custom scalar."
                ))
            })?;
            Value::Int(value)
        }
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
