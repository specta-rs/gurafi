//! Schema construction, validation and lookup.

use core::fmt;
use std::sync::Arc;

use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;

use crate::{
    error::GraphQLError,
    types::{
        DirectiveDefinition, EnumType, Field, InputObjectType, InputValue, InterfaceType, Name,
        ObjectType, ScalarType, Type, TypeKind, TypeRef, UnionType,
    },
};

/// An error produced while building a schema.
#[derive(Clone, Debug, Default)]
pub struct SchemaError {
    /// Every problem found while validating the schema.
    pub errors: Vec<GraphQLError>,
}

impl SchemaError {
    /// Create an error from a single message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            errors: vec![GraphQLError::new(message.into())],
        }
    }

    /// Whether no errors are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// The number of errors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    pub(crate) fn push(&mut self, message: impl Into<String>) {
        self.errors.push(GraphQLError::new(message.into()));
    }

    /// Consume this value into the underlying errors.
    #[must_use]
    pub fn into_errors(self) -> Vec<GraphQLError> {
        self.errors
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SchemaError {}

impl From<GraphQLError> for SchemaError {
    fn from(value: GraphQLError) -> Self {
        Self {
            errors: vec![value],
        }
    }
}

/// A name-keyed map that preserves insertion order and hashes cheaply.
///
/// GraphQL schemas look up a definition by name far more often than they resize, so the
/// cryptographic-quality default hasher is pure overhead here. `FxHash` is a well-distributed
/// multiply-xor hash; it is not collision resistant, which is fine for a schema built from
/// trusted definitions (and for documents, which `collect` already hashes with `FxHash`).
pub(crate) type TypeMap<V> = IndexMap<Name, V, FxBuildHasher>;

#[derive(Debug)]
pub(crate) struct SchemaInner {
    /// Types in definition order, hashed with `FxHash`.
    ///
    /// Execution looks up a type for nearly every completed field, so the schema-wide map must
    /// not pay for a cryptographic hash. Insertion order is what `Schema::types` and the
    /// introspection response rely on, and it is unchanged.
    pub(crate) types: TypeMap<Type>,
    pub(crate) query: Name,
    pub(crate) mutation: Option<Name>,
    pub(crate) subscription: Option<Name>,
    pub(crate) directives: TypeMap<DirectiveDefinition>,
    pub(crate) description: Option<Arc<str>>,
}

/// A validated, immutable GraphQL schema.
///
/// Cloning a schema is cheap: every definition lives behind an `Arc` inside one shared
/// allocation.
#[derive(Clone, Debug)]
pub struct Schema {
    pub(crate) inner: Arc<SchemaInner>,
}

impl Schema {
    /// Start building a schema.
    #[must_use]
    pub fn builder() -> SchemaBuilder {
        SchemaBuilder::new()
    }

    /// Create a schema builder that already contains the default root type names.
    #[must_use]
    pub fn new() -> SchemaBuilder {
        Self::builder()
    }

    /// The schema's optional description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }

    /// Look up a named type.
    #[must_use]
    pub fn get_type(&self, name: &str) -> Option<&Type> {
        self.inner.types.get(name)
    }

    /// Iterate over all types, in definition order.
    pub fn types(&self) -> impl ExactSizeIterator<Item = (&Name, &Type)> {
        self.inner.types.iter()
    }

    /// Iterate over all directive definitions.
    pub fn directives(&self) -> impl ExactSizeIterator<Item = (&Name, &DirectiveDefinition)> {
        self.inner.directives.iter()
    }

    /// Look up a directive definition.
    #[must_use]
    pub fn get_directive(&self, name: &str) -> Option<&DirectiveDefinition> {
        self.inner.directives.get(name)
    }

    /// The query root type name.
    #[must_use]
    pub fn query_name(&self) -> &Name {
        &self.inner.query
    }

    /// The mutation root type name, when mutations are enabled.
    #[must_use]
    pub fn mutation_name(&self) -> Option<&Name> {
        self.inner.mutation.as_ref()
    }

    /// The subscription root type name, when subscriptions are enabled.
    #[must_use]
    pub fn subscription_name(&self) -> Option<&Name> {
        self.inner.subscription.as_ref()
    }

    /// The query root object type.
    ///
    /// # Panics
    ///
    /// Never in practice: the schema builder rejects a query root that is not an object.
    #[must_use]
    pub fn query_type(&self) -> &Arc<ObjectType> {
        self.object_type(&self.inner.query)
            .expect("validated schema has an object query root")
    }

    /// The mutation root object type, when present.
    #[must_use]
    pub fn mutation_type(&self) -> Option<&Arc<ObjectType>> {
        self.inner
            .mutation
            .as_ref()
            .and_then(|name| self.object_type(name))
    }

    /// The subscription root object type, when present.
    #[must_use]
    pub fn subscription_type(&self) -> Option<&Arc<ObjectType>> {
        self.inner
            .subscription
            .as_ref()
            .and_then(|name| self.object_type(name))
    }

    /// Look up an object type by name.
    #[must_use]
    pub fn object_type(&self, name: &str) -> Option<&Arc<ObjectType>> {
        self.get_type(name).and_then(Type::as_object)
    }

    /// Look up an interface type by name.
    #[must_use]
    pub fn interface_type(&self, name: &str) -> Option<&Arc<InterfaceType>> {
        self.get_type(name).and_then(Type::as_interface)
    }

    /// Look up a union type by name.
    #[must_use]
    pub fn union_type(&self, name: &str) -> Option<&Arc<UnionType>> {
        self.get_type(name).and_then(Type::as_union)
    }

    /// Look up an enum type by name.
    #[must_use]
    pub fn enum_type(&self, name: &str) -> Option<&Arc<EnumType>> {
        self.get_type(name).and_then(Type::as_enum)
    }

    /// Look up an input object type by name.
    #[must_use]
    pub fn input_object_type(&self, name: &str) -> Option<&Arc<InputObjectType>> {
        self.get_type(name).and_then(Type::as_input_object)
    }

    /// Look up a scalar type by name.
    #[must_use]
    pub fn scalar_type(&self, name: &str) -> Option<&Arc<ScalarType>> {
        self.get_type(name).and_then(Type::as_scalar)
    }

    /// Resolve a type reference's innermost named type.
    #[must_use]
    pub fn type_from_ref<'a>(&'a self, ty: &TypeRef) -> Option<&'a Type> {
        self.get_type(ty.named_type())
    }

    /// Whether a concrete object type implements an interface, directly or transitively.
    #[must_use]
    pub fn object_implements(&self, object: &ObjectType, interface: &str) -> bool {
        // Most objects implement their interfaces directly, and most interfaces implement
        // nothing, so the common questions are answered without building a work list.
        if object.interfaces.iter().any(|name| &**name == interface) {
            return true;
        }
        let mut stack: smallvec::SmallVec<[&Name; 4]> = object.interfaces.iter().collect();
        while let Some(name) = stack.pop() {
            if &**name == interface {
                return true;
            }
            if let Some(ty) = self.interface_type(name) {
                stack.extend(ty.interfaces.iter());
            }
        }
        false
    }

    /// Whether a type is a possible type of an abstract type.
    #[must_use]
    pub fn is_possible_type(&self, abstract_type: &str, object_type: &str) -> bool {
        match self.get_type(abstract_type) {
            Some(Type::Union(union)) => union.types.iter().any(|ty| &**ty == object_type),
            Some(Type::Interface(_)) => self
                .object_type(object_type)
                .is_some_and(|object| self.object_implements(object, abstract_type)),
            _ => false,
        }
    }

    /// Every object type that can appear where the abstract type is expected.
    #[must_use]
    pub fn possible_types(&self, abstract_type: &str) -> Vec<Name> {
        match self.get_type(abstract_type) {
            Some(Type::Union(union)) => union.types.clone(),
            Some(Type::Interface(_)) => self
                .inner
                .types
                .values()
                .filter_map(|ty| match ty {
                    Type::Object(object) if self.object_implements(object, abstract_type) => {
                        Some(object.name.clone())
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The rendered GraphQL schema definition language document.
    #[must_use]
    pub fn sdl(&self) -> String {
        crate::sdl::print_schema(self)
    }

    /// Build a schema from a GraphQL SDL document.
    ///
    /// The resulting schema has no resolvers; it is intended for tooling, compatibility checks
    /// and introspection snapshots.
    ///
    /// # Errors
    ///
    /// Returns parse and schema validation errors.
    pub fn from_sdl(source: &str) -> Result<Self, SchemaError> {
        crate::sdl_schema::schema_from_sdl(source)
    }
}

/// Builds and validates a [`Schema`].
///
/// The builder is mutable and explicit. Higher-level crates such as `gurafi` provide a more
/// ergonomic API on top; this one only cares about the specification.
#[derive(Debug)]
pub struct SchemaBuilder {
    types: TypeMap<Type>,
    query: Option<Name>,
    mutation: Option<Name>,
    subscription: Option<Name>,
    directives: TypeMap<DirectiveDefinition>,
    description: Option<Arc<str>>,
    errors: SchemaError,
}

impl Default for SchemaBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaBuilder {
    /// Create a builder pre-populated with the built-in scalars and directives.
    #[must_use]
    pub fn new() -> Self {
        let mut types = TypeMap::default();
        for scalar in [
            ScalarType::string(),
            ScalarType::int(),
            ScalarType::float(),
            ScalarType::boolean(),
            ScalarType::id(),
        ] {
            let name = scalar.name.clone();
            types.insert(name, Type::Scalar(Arc::new(scalar)));
        }

        let mut directives = TypeMap::default();
        for directive in [
            DirectiveDefinition::skip(),
            DirectiveDefinition::include(),
            DirectiveDefinition::deprecated(),
            DirectiveDefinition::specified_by(),
            DirectiveDefinition::one_of(),
        ] {
            directives.insert(directive.name.clone(), directive);
        }

        Self {
            types,
            query: None,
            mutation: None,
            subscription: None,
            directives,
            description: None,
            errors: SchemaError::default(),
        }
    }

    /// Set the schema description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the query root type by name.
    #[must_use]
    pub fn query(mut self, name: impl Into<Name>) -> Self {
        self.query = Some(name.into());
        self
    }

    /// Set the mutation root type by name.
    #[must_use]
    pub fn mutation(mut self, name: impl Into<Name>) -> Self {
        self.mutation = Some(name.into());
        self
    }

    /// Set the subscription root type by name.
    #[must_use]
    pub fn subscription(mut self, name: impl Into<Name>) -> Self {
        self.subscription = Some(name.into());
        self
    }

    /// Register a scalar.
    #[must_use]
    pub fn scalar(mut self, scalar: ScalarType) -> Self {
        self.add_type(Type::Scalar(Arc::new(scalar)));
        self
    }

    /// Register an object type.
    #[must_use]
    pub fn object(mut self, object: ObjectType) -> Self {
        self.add_type(Type::Object(Arc::new(object)));
        self
    }

    /// Register an interface type.
    #[must_use]
    pub fn interface(mut self, interface: InterfaceType) -> Self {
        self.add_type(Type::Interface(Arc::new(interface)));
        self
    }

    /// Register a union type.
    #[must_use]
    pub fn union(mut self, union: UnionType) -> Self {
        self.add_type(Type::Union(Arc::new(union)));
        self
    }

    /// Register an enum type.
    #[must_use]
    pub fn enumeration(mut self, enumeration: EnumType) -> Self {
        self.add_type(Type::Enum(Arc::new(enumeration)));
        self
    }

    /// Register an input object type.
    #[must_use]
    pub fn input_object(mut self, input: InputObjectType) -> Self {
        self.add_type(Type::InputObject(Arc::new(input)));
        self
    }

    /// Register an arbitrary type definition.
    #[must_use]
    pub fn register(mut self, ty: Type) -> Self {
        self.add_type(ty);
        self
    }

    /// Register an arbitrary type definition, taking `&mut self` for builder style code.
    pub fn add_type(&mut self, ty: Type) -> &mut Self {
        let name = ty.name().clone();
        if name.starts_with("__") {
            self.errors.push(format!(
                "Type name \"{name}\" must not begin with \"__\", which is reserved by GraphQL introspection."
            ));
            return self;
        }
        if self.types.contains_key(&name) {
            self.errors
                .push(format!("There can be only one type named \"{name}\"."));
            return self;
        }
        self.types.insert(name, ty);
        self
    }

    /// Register a directive definition.
    #[must_use]
    pub fn directive(mut self, definition: DirectiveDefinition) -> Self {
        self.add_directive(definition);
        self
    }

    /// Register a directive definition, taking `&mut self`.
    pub fn add_directive(&mut self, definition: DirectiveDefinition) -> &mut Self {
        let name = definition.name.clone();
        if name.starts_with("__") {
            self.errors.push(format!(
                "Directive name \"@{name}\" must not begin with \"__\", which is reserved by GraphQL introspection."
            ));
            return self;
        }
        if self.directives.contains_key(&name) {
            self.errors.push(format!(
                "There can be only one directive named \"@{name}\"."
            ));
            return self;
        }
        self.directives.insert(name, definition);
        self
    }

    /// Whether a type with the given name is already registered.
    #[must_use]
    pub fn has_type(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    /// Every error recorded so far.
    #[must_use]
    pub fn errors(&self) -> &[GraphQLError] {
        &self.errors.errors
    }

    /// Validate and freeze the schema.
    ///
    /// # Errors
    ///
    /// Returns every validation problem found. The GraphQL specification requires a schema to
    /// reject duplicate types, dangling references, invalid interface implementations and
    /// invalid input cycles; all of those are reported here rather than deferred to execution.
    pub fn build(mut self) -> Result<Schema, SchemaError> {
        // Introspection types are always present. Users cannot register them, so no duplicate
        // check is needed.
        if self.types.get("__Schema").is_none() {
            for ty in crate::introspection::types() {
                let name = ty.name().clone();
                self.types.insert(name, ty);
            }
        }

        let query = self.query.clone().or_else(|| {
            if self.types.contains_key("Query") {
                Some("Query".into())
            } else {
                None
            }
        });
        let Some(query) = query else {
            self.errors
                .push("A schema must define a query root type.".to_owned());
            return Err(self.errors);
        };
        if self.types.get(&query).is_none() {
            self.errors
                .push(format!("The query root type \"{query}\" does not exist."));
        }
        if let Some(mutation) = &self.mutation {
            if self.types.get(mutation).is_none() {
                self.errors.push(format!(
                    "The mutation root type \"{mutation}\" does not exist."
                ));
            }
        }
        if let Some(subscription) = &self.subscription {
            if self.types.get(subscription).is_none() {
                self.errors.push(format!(
                    "The subscription root type \"{subscription}\" does not exist."
                ));
            }
        }

        let inner = SchemaInner {
            types: self.types,
            query,
            mutation: self.mutation,
            subscription: self.subscription,
            directives: self.directives,
            description: self.description,
        };
        let schema = Schema {
            inner: Arc::new(inner),
        };

        validate_schema(&schema, &mut self.errors);

        if self.errors.is_empty() {
            Ok(schema)
        } else {
            Err(self.errors)
        }
    }
}

/// Check that an output type reference is structurally valid and names an output type.
fn validate_output_type(
    schema: &Schema,
    reference: &TypeRef,
    owner: &str,
    errors: &mut SchemaError,
) {
    match reference {
        TypeRef::NonNull(inner) => {
            if matches!(**inner, TypeRef::NonNull(_)) {
                errors.push(format!(
                    "{owner} uses a non-null wrapper around another non-null type."
                ));
            }
            validate_output_type(schema, inner, owner, errors);
        }
        TypeRef::List(inner) => validate_output_type(schema, inner, owner, errors),
        TypeRef::Named(name) => match schema.get_type(name) {
            None => errors.push(format!("{owner} references an unknown type \"{name}\".")),
            Some(ty) if !ty.is_output_type() => errors.push(format!(
                "{owner} must use an output type but \"{name}\" is an input object."
            )),
            Some(_) => {}
        },
    }
}

/// Check that an input type reference is structurally valid and names an input type.
fn validate_input_type(
    schema: &Schema,
    reference: &TypeRef,
    owner: &str,
    errors: &mut SchemaError,
) {
    match reference {
        TypeRef::NonNull(inner) => {
            if matches!(**inner, TypeRef::NonNull(_)) {
                errors.push(format!(
                    "{owner} uses a non-null wrapper around another non-null type."
                ));
            }
            validate_input_type(schema, inner, owner, errors);
        }
        TypeRef::List(inner) => validate_input_type(schema, inner, owner, errors),
        TypeRef::Named(name) => match schema.get_type(name) {
            None => errors.push(format!("{owner} references an unknown type \"{name}\".")),
            Some(ty) if !ty.is_input_type() => errors.push(format!(
                "{owner} must use an input type but \"{name}\" is not one."
            )),
            Some(_) => {}
        },
    }
}

fn validate_arguments(
    schema: &Schema,
    args: &IndexMap<Name, InputValue>,
    owner: &str,
    errors: &mut SchemaError,
) {
    for argument in args.values() {
        validate_input_type(
            schema,
            &argument.ty,
            &format!("Argument \"{}\" of {owner}", argument.name),
            errors,
        );
        if let Some(default) = &argument.default_value {
            if let Err(error) =
                crate::execution::coerce::validate_default_value(schema, default, &argument.ty)
            {
                errors.push(format!(
                    "Default value for argument \"{}\" of {owner} is invalid: {}",
                    argument.name,
                    error.message()
                ));
            }
        }
        if argument.name.starts_with("__") {
            errors.push(format!(
                "Argument \"{}\" of {owner} must not begin with \"__\".",
                argument.name
            ));
        }
        if argument.deprecation_reason.is_some()
            && matches!(argument.ty, TypeRef::NonNull(_))
            && argument.default_value.is_none()
        {
            errors.push(format!(
                "Required argument \"{}\" of {owner} cannot be deprecated.",
                argument.name
            ));
        }
    }
}

fn validate_schema(schema: &Schema, errors: &mut SchemaError) {
    for ty in schema.inner.types.values() {
        match ty {
            Type::Scalar(_) => {}
            Type::Object(object) => {
                if object.fields.is_empty() {
                    errors.push(format!(
                        "Object type \"{}\" must define at least one field.",
                        object.name
                    ));
                }
                for duplicate in &object.duplicates {
                    errors.push(format!(
                        "Object \"{}\" defines field \"{duplicate}\" more than once.",
                        object.name
                    ));
                }
                for field in object.fields.values() {
                    validate_field(schema, field, &object.name, errors);
                }
                for interface in &object.interfaces {
                    match schema.get_type(interface) {
                        Some(Type::Interface(interface_ty)) => {
                            // Every field on the interface must be present with a compatible type
                            // and every required argument must be accepted.
                            for (field_name, interface_field) in &interface_ty.fields {
                                match object.fields.get(field_name) {
                                    None => errors.push(format!(
                                        "Interface field \"{}.{field_name}\" expected but \"{}\" does not provide it.",
                                        interface_ty.name, object.name
                                    )),
                                    Some(object_field) => {
                                        for (arg_name, object_arg) in &object_field.args {
                                            if !interface_field.args.contains_key(arg_name)
                                                && matches!(object_arg.ty, TypeRef::NonNull(_))
                                                && object_arg.default_value.is_none()
                                            {
                                                errors.push(format!(
                                                    "Object field \"{}.{field_name}\" adds required argument \"{arg_name}\" which is not present on interface field \"{}.{field_name}\".",
                                                    object.name, interface_ty.name
                                                ));
                                            }
                                        }
                                        if !is_output_subtype(
                                            schema,
                                            &object_field.ty,
                                            &interface_field.ty,
                                        ) {
                                            errors.push(format!(
                                                "Interface field \"{}.{field_name}\" expects type \"{}\" but \"{}.{field_name}\" is type \"{}\".",
                                                interface_ty.name,
                                                interface_field.ty,
                                                object.name,
                                                object_field.ty
                                            ));
                                        }
                                        for (arg_name, interface_arg) in &interface_field.args {
                                            match object_field.args.get(arg_name) {
                                                None => errors.push(format!(
                                                    "Interface field argument \"{}.{field_name}({arg_name}:)\" expected but \"{}.{field_name}\" does not provide it.",
                                                    interface_ty.name, object.name
                                                )),
                                                Some(object_arg) => {
                                                    if object_arg.ty != interface_arg.ty {
                                                        errors.push(format!(
                                                            "Interface field argument \"{}.{field_name}({arg_name}:)\" expects type \"{}\" but \"{}.{field_name}({arg_name}:)\" is type \"{}\".",
                                                            interface_ty.name,
                                                            interface_arg.ty,
                                                            object.name,
                                                            object_arg.ty
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(_) => errors.push(format!(
                            "Type \"{}\" must only implement interfaces, but \"{interface}\" is not an interface.",
                            object.name
                        )),
                        None => errors.push(format!(
                            "Type \"{}\" implements unknown interface \"{interface}\".",
                            object.name
                        )),
                    }
                }
                for (index, interface) in object.interfaces.iter().enumerate() {
                    if object.interfaces[index + 1..].contains(interface) {
                        errors.push(format!(
                            "Type \"{}\" implements interface \"{interface}\" more than once.",
                            object.name
                        ));
                    }
                    if let Some(interface_ty) = schema.interface_type(interface) {
                        for transitive in &interface_ty.interfaces {
                            if !object.interfaces.contains(transitive) {
                                errors.push(format!(
                                    "Type \"{}\" must implement interface \"{transitive}\" because it implements \"{interface}\".",
                                    object.name
                                ));
                            }
                        }
                    }
                }
            }
            Type::Interface(interface) => {
                if interface.fields.is_empty() {
                    errors.push(format!(
                        "Interface type \"{}\" must define at least one field.",
                        interface.name
                    ));
                }
                for duplicate in &interface.duplicates {
                    errors.push(format!(
                        "Interface \"{}\" defines field \"{duplicate}\" more than once.",
                        interface.name
                    ));
                }
                for field in interface.fields.values() {
                    validate_field(schema, field, &interface.name, errors);
                }
                for implemented in &interface.interfaces {
                    match schema.get_type(implemented) {
                        Some(Type::Interface(parent)) => {
                            for (field_name, parent_field) in &parent.fields {
                                match interface.fields.get(field_name) {
                                    None => errors.push(format!(
                                        "Interface field \"{}.{field_name}\" expected but \"{}\" does not provide it.",
                                        parent.name, interface.name
                                    )),
                                    Some(field) => {
                                        for (arg_name, parent_arg) in &parent_field.args {
                                            match field.args.get(arg_name) {
                                                None => errors.push(format!(
                                                    "Interface field argument \"{}.{field_name}({arg_name}:)\" expected but \"{}.{field_name}\" does not provide it.",
                                                    parent.name, interface.name
                                                )),
                                                Some(arg) if arg.ty != parent_arg.ty => errors.push(format!(
                                                    "Interface field argument \"{}.{field_name}({arg_name}:)\" expects type \"{}\" but \"{}.{field_name}({arg_name}:)\" is type \"{}\".",
                                                    parent.name, parent_arg.ty, interface.name, arg.ty
                                                )),
                                                Some(_) => {}
                                            }
                                        }
                                        for (arg_name, child_arg) in &field.args {
                                            if !parent_field.args.contains_key(arg_name)
                                                && matches!(child_arg.ty, TypeRef::NonNull(_))
                                                && child_arg.default_value.is_none()
                                            {
                                                errors.push(format!(
                                                    "Interface field \"{}.{field_name}\" adds required argument \"{arg_name}\" which is not present on \"{}.{field_name}\".",
                                                    interface.name, parent.name
                                                ));
                                            }
                                        }
                                        if !is_output_subtype(schema, &field.ty, &parent_field.ty)
                                        {
                                            errors.push(format!(
                                                "Interface field \"{}.{field_name}\" expects type \"{}\" but \"{}.{field_name}\" is type \"{}\".",
                                                parent.name,
                                                parent_field.ty,
                                                interface.name,
                                                field.ty
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        Some(_) => errors.push(format!(
                            "Interface \"{}\" must only implement interfaces, but \"{implemented}\" is not an interface.",
                            interface.name
                        )),
                        None => errors.push(format!(
                            "Interface \"{}\" implements unknown interface \"{implemented}\".",
                            interface.name
                        )),
                    }
                }
                for (index, implemented) in interface.interfaces.iter().enumerate() {
                    if interface.interfaces[index + 1..].contains(implemented) {
                        errors.push(format!(
                            "Interface \"{}\" implements interface \"{implemented}\" more than once.",
                            interface.name
                        ));
                    }
                    if implemented == &interface.name {
                        errors.push(format!(
                            "Interface \"{}\" must not implement itself.",
                            interface.name
                        ));
                    }
                }
                // Walk the interface graph to catch longer cycles.
                let mut stack: Vec<Name> = interface.interfaces.clone();
                let mut visited: Vec<Name> = Vec::new();
                while let Some(current) = stack.pop() {
                    if current == interface.name {
                        errors.push(format!(
                            "Interface \"{}\" must not implement itself through another interface.",
                            interface.name
                        ));
                        break;
                    }
                    if visited.contains(&current) {
                        continue;
                    }
                    visited.push(current.clone());
                    if let Some(parent) = schema.interface_type(&current) {
                        stack.extend(parent.interfaces.iter().cloned());
                    }
                }
            }
            Type::Union(union) => {
                if union.types.is_empty() {
                    errors.push(format!(
                        "Union \"{}\" must include at least one object type.",
                        union.name
                    ));
                }
                let mut seen = Vec::new();
                for member in &union.types {
                    match schema.get_type(member) {
                        Some(Type::Object(_)) => {}
                        Some(_) => errors.push(format!(
                            "Union \"{}\" can only include object types, but \"{member}\" is not an object.",
                            union.name
                        )),
                        None => errors.push(format!(
                            "Union \"{}\" includes unknown type \"{member}\".",
                            union.name
                        )),
                    }
                    if seen.contains(member) {
                        errors.push(format!(
                            "Union \"{}\" can only include type \"{member}\" once.",
                            union.name
                        ));
                    } else {
                        seen.push(member.clone());
                    }
                }
            }
            Type::Enum(enumeration) => {
                for duplicate in &enumeration.duplicates {
                    errors.push(format!(
                        "Enum \"{}\" defines value \"{duplicate}\" more than once.",
                        enumeration.name
                    ));
                }
                if enumeration.values.is_empty() {
                    errors.push(format!(
                        "Enum \"{}\" must define at least one value.",
                        enumeration.name
                    ));
                }
                for value in enumeration.values.values() {
                    if value.name.starts_with("__") {
                        errors.push(format!(
                            "Enum value \"{}.{}\" must not begin with \"__\".",
                            enumeration.name, value.name
                        ));
                    }
                }
            }
            Type::InputObject(input) => {
                for duplicate in &input.duplicates {
                    errors.push(format!(
                        "Input object \"{}\" defines field \"{duplicate}\" more than once.",
                        input.name
                    ));
                }
                if input.fields.is_empty() {
                    errors.push(format!(
                        "Input object \"{}\" must define at least one field.",
                        input.name
                    ));
                }
                for field in input.fields.values() {
                    validate_input_type(
                        schema,
                        &field.ty,
                        &format!("Field \"{}.{}\"", input.name, field.name),
                        errors,
                    );
                    if let Some(default) = &field.default_value {
                        if let Err(error) = crate::execution::coerce::validate_default_value(
                            schema, default, &field.ty,
                        ) {
                            errors.push(format!(
                                "Default value for input field \"{}.{}\" is invalid: {}",
                                input.name,
                                field.name,
                                error.message()
                            ));
                        }
                    }
                    if field.name.starts_with("__") {
                        errors.push(format!(
                            "Input field \"{}.{}\" must not begin with \"__\".",
                            input.name, field.name
                        ));
                    }
                    if field.deprecation_reason.is_some()
                        && matches!(field.ty, TypeRef::NonNull(_))
                        && field.default_value.is_none()
                    {
                        errors.push(format!(
                            "Required input field \"{}.{}\" cannot be deprecated.",
                            input.name, field.name
                        ));
                    }
                    if input.one_of {
                        if matches!(field.ty, TypeRef::NonNull(_)) {
                            errors.push(format!(
                                "OneOf input field \"{}.{}\" must be nullable.",
                                input.name, field.name
                            ));
                        }
                        if field.default_value.is_some() {
                            errors.push(format!(
                                "OneOf input field \"{}.{}\" must not have a default value.",
                                input.name, field.name
                            ));
                        }
                    }
                }
            }
        }
    }

    for directive in schema.inner.directives.values() {
        for duplicate in &directive.duplicate_args {
            errors.push(format!(
                "Directive \"@{}\" defines argument \"{duplicate}\" more than once.",
                directive.name
            ));
        }
        if directive.locations.is_empty() {
            errors.push(format!(
                "Directive \"@{}\" must include at least one location.",
                directive.name
            ));
        }
        validate_arguments(
            schema,
            &directive.args,
            &format!("directive \"@{}\"", directive.name),
            errors,
        );
    }

    if let Some(mutation) = &schema.inner.mutation {
        if &schema.inner.query == mutation {
            errors.push("The query and mutation root types must be different.".to_owned());
        }
    }
    if let Some(subscription) = &schema.inner.subscription {
        if subscription == &schema.inner.query
            || schema.inner.mutation.as_ref() == Some(subscription)
        {
            errors.push(
                "The subscription root type must be different from the other roots.".to_owned(),
            );
        }
    }

    // Root types must be objects. This is checked after the type loop so that missing types are
    // reported once above.
    for (kind, name) in [
        ("query", Some(&schema.inner.query)),
        ("mutation", schema.inner.mutation.as_ref()),
        ("subscription", schema.inner.subscription.as_ref()),
    ] {
        let Some(name) = name else { continue };
        match schema.get_type(name) {
            Some(Type::Object(_)) => {}
            Some(ty) => errors.push(format!(
                "The {kind} root type \"{name}\" must be an object, but it is {}.",
                ty.kind()
            )),
            None => {}
        }
    }

    detect_input_cycles(schema, errors);
}

fn validate_field(schema: &Schema, field: &Field, owner: &str, errors: &mut SchemaError) {
    for duplicate in &field.duplicate_args {
        errors.push(format!(
            "Field \"{owner}.{}\" defines argument \"{duplicate}\" more than once.",
            field.name
        ));
    }
    validate_output_type(
        schema,
        &field.ty,
        &format!("Field \"{owner}.{}\"", field.name),
        errors,
    );
    validate_arguments(
        schema,
        &field.args,
        &format!("field \"{owner}.{}\"", field.name),
        errors,
    );
    if field.name.starts_with("__") {
        errors.push(format!(
            "Field \"{owner}.{}\" must not begin with \"__\".",
            field.name
        ));
    }
    for argument in field.args.values() {
        if matches!(argument.ty, TypeRef::NonNull(_)) && argument.default_value.is_none() {
            // This is legal, but introspection and validation both care about the distinction.
        }
    }
}

/// Output type covariance, as required for interface fields and for schema evolution checks.
#[must_use]
pub fn is_output_subtype(schema: &Schema, sub: &TypeRef, sup: &TypeRef) -> bool {
    match (sub, sup) {
        // A non-null type may appear where a nullable type is expected: `ID!` is a subtype of
        // `ID`, and `[Int!]!` is a subtype of `[Int!]`.
        (_, TypeRef::NonNull(sup_inner)) => match sub {
            TypeRef::NonNull(sub_inner) => is_output_subtype(schema, sub_inner, sup_inner),
            _ => false,
        },
        (TypeRef::NonNull(sub_inner), _) => is_output_subtype(schema, sub_inner, sup),
        (TypeRef::List(sub), TypeRef::List(sup)) => is_output_subtype(schema, sub, sup),
        (TypeRef::List(_), _) | (_, TypeRef::List(_)) => false,
        (TypeRef::Named(sub), TypeRef::Named(sup)) => {
            if sub == sup {
                return true;
            }
            let Some(sub_ty) = schema.get_type(sub) else {
                return false;
            };
            let Some(sup_ty) = schema.get_type(sup) else {
                return false;
            };
            match (sub_ty, sup_ty) {
                (Type::Object(object), Type::Interface(_)) => schema.object_implements(object, sup),
                (Type::Object(object), Type::Union(union)) => {
                    union.types.iter().any(|ty| ty == &object.name)
                }
                (Type::Interface(sub_interface), Type::Interface(_)) => {
                    let mut stack = sub_interface.interfaces.clone();
                    while let Some(name) = stack.pop() {
                        if name.as_ref() == sup.as_ref() {
                            return true;
                        }
                        if let Some(parent) = schema.interface_type(&name) {
                            stack.extend(parent.interfaces.iter().cloned());
                        }
                    }
                    false
                }
                _ => false,
            }
        }
    }
}

/// Whether a field is a non-null, non-list reference. Only those edges can make an input cycle
/// impossible to satisfy; a list can be empty and a nullable field can be null.
fn is_non_null_singular(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::NonNull(inner) => !inner.is_list(),
        _ => false,
    }
}

fn detect_input_cycles(schema: &Schema, errors: &mut SchemaError) {
    // A cycle is only invalid when every edge in it is a non-null, non-list field. Nullable and
    // list fields provide an escape hatch, so they break the cycle.
    fn visit(schema: &Schema, name: &str, path: &mut Vec<Name>, errors: &mut SchemaError) {
        if path.iter().any(|item| &**item == name) {
            let cycle = path
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            errors.push(format!(
                "Input object cycle detected: {cycle} -> {name}. Add a nullable or list field to break the cycle."
            ));
            return;
        }
        let Some(input) = schema.input_object_type(name) else {
            return;
        };
        path.push(input.name.clone());
        for field in input.fields.values() {
            if is_non_null_singular(&field.ty) {
                visit(schema, field.ty.named_type(), path, errors);
            }
        }
        path.pop();
    }

    for (name, ty) in &schema.inner.types {
        if matches!(ty, Type::InputObject(_)) {
            visit(schema, name, &mut Vec::new(), errors);
        }
    }
}

/// A schema difference that can break existing clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaChange {
    /// A type was removed.
    TypeRemoved {
        /// The type name.
        name: String,
    },
    /// A type changed kind, for example object to scalar.
    TypeKindChanged {
        /// The type name.
        name: String,
        /// The old kind.
        from: TypeKind,
        /// The new kind.
        to: TypeKind,
    },
    /// A field was removed from an object or interface.
    FieldRemoved {
        /// The type name.
        type_name: String,
        /// The field name.
        field_name: String,
    },
    /// A field's type changed in a way existing queries may not handle.
    FieldTypeChanged {
        /// The type name.
        type_name: String,
        /// The field name.
        field_name: String,
        /// The old type.
        from: TypeRef,
        /// The new type.
        to: TypeRef,
    },
    /// A required argument was added without a default value.
    RequiredArgumentAdded {
        /// The type name.
        type_name: String,
        /// The field name.
        field_name: String,
        /// The argument name.
        argument_name: String,
    },
    /// An argument was removed.
    ArgumentRemoved {
        /// The type name.
        type_name: String,
        /// The field name.
        field_name: String,
        /// The argument name.
        argument_name: String,
    },
    /// An argument's type changed.
    ArgumentTypeChanged {
        /// The type name.
        type_name: String,
        /// The field name.
        field_name: String,
        /// The argument name.
        argument_name: String,
        /// The old type.
        from: TypeRef,
        /// The new type.
        to: TypeRef,
    },
    /// An enum value was removed.
    EnumValueRemoved {
        /// The enum name.
        enum_name: String,
        /// The value.
        value: String,
    },
    /// An interface was removed from an object's implemented interfaces.
    InterfaceRemoved {
        /// The object name.
        object_name: String,
        /// The interface name.
        interface_name: String,
    },
    /// A union member was removed.
    UnionMemberRemoved {
        /// The union name.
        union_name: String,
        /// The member type name.
        member: String,
    },
    /// An input object field was removed.
    InputFieldRemoved {
        /// The input object name.
        input_name: String,
        /// The field name.
        field_name: String,
    },
    /// A required input field became required, or a new required field was added.
    InputFieldBecameRequired {
        /// The input object name.
        input_name: String,
        /// The field name.
        field_name: String,
    },
    /// A directive was removed.
    DirectiveRemoved {
        /// The directive name.
        name: String,
    },
    /// A directive location was removed.
    DirectiveLocationRemoved {
        /// The directive name.
        name: String,
        /// The location.
        location: &'static str,
    },
    /// An argument was removed from a directive definition.
    DirectiveArgumentRemoved {
        /// The directive name.
        directive: String,
        /// The argument name.
        argument: String,
    },
    /// A directive argument's type changed.
    DirectiveArgumentTypeChanged {
        /// The directive name.
        directive: String,
        /// The argument name.
        argument: String,
        /// The old type.
        from: TypeRef,
        /// The new type.
        to: TypeRef,
    },
    /// An interface stopped implementing another interface.
    InterfaceImplementationRemoved {
        /// The interface name.
        interface_name: String,
        /// The interface it no longer implements.
        implemented: String,
    },
}

impl fmt::Display for SchemaChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeRemoved { name } => write!(f, "type \"{name}\" was removed"),
            Self::TypeKindChanged { name, from, to } => {
                write!(f, "type \"{name}\" changed from {from} to {to}")
            }
            Self::FieldRemoved {
                type_name,
                field_name,
            } => write!(f, "field \"{type_name}.{field_name}\" was removed"),
            Self::FieldTypeChanged {
                type_name,
                field_name,
                from,
                to,
            } => write!(
                f,
                "field \"{type_name}.{field_name}\" changed type from \"{from}\" to \"{to}\""
            ),
            Self::RequiredArgumentAdded {
                type_name,
                field_name,
                argument_name,
            } => write!(
                f,
                "required argument \"{type_name}.{field_name}({argument_name}:)\" was added"
            ),
            Self::ArgumentRemoved {
                type_name,
                field_name,
                argument_name,
            } => write!(
                f,
                "argument \"{type_name}.{field_name}({argument_name}:)\" was removed"
            ),
            Self::ArgumentTypeChanged {
                type_name,
                field_name,
                argument_name,
                from,
                to,
            } => write!(
                f,
                "argument \"{type_name}.{field_name}({argument_name}:)\" changed type from \"{from}\" to \"{to}\""
            ),
            Self::EnumValueRemoved { enum_name, value } => {
                write!(f, "enum value \"{enum_name}.{value}\" was removed")
            }
            Self::InterfaceRemoved {
                object_name,
                interface_name,
            } => write!(
                f,
                "object \"{object_name}\" no longer implements interface \"{interface_name}\""
            ),
            Self::UnionMemberRemoved { union_name, member } => {
                write!(f, "union \"{union_name}\" no longer includes \"{member}\"")
            }
            Self::InputFieldRemoved {
                input_name,
                field_name,
            } => write!(f, "input field \"{input_name}.{field_name}\" was removed"),
            Self::InputFieldBecameRequired {
                input_name,
                field_name,
            } => write!(
                f,
                "input field \"{input_name}.{field_name}\" became required"
            ),
            Self::DirectiveRemoved { name } => write!(f, "directive \"@{name}\" was removed"),
            Self::DirectiveLocationRemoved { name, location } => {
                write!(f, "directive \"@{name}\" no longer supports {location}")
            }
            Self::DirectiveArgumentRemoved {
                directive,
                argument,
            } => {
                write!(
                    f,
                    "directive \"@{directive}\" no longer accepts argument \"{argument}\""
                )
            }
            Self::DirectiveArgumentTypeChanged {
                directive,
                argument,
                from,
                to,
            } => write!(
                f,
                "directive \"@{directive}\" argument \"{argument}\" changed type from \"{from}\" to \"{to}\""
            ),
            Self::InterfaceImplementationRemoved {
                interface_name,
                implemented,
            } => write!(
                f,
                "interface \"{interface_name}\" no longer implements \"{implemented}\""
            ),
        }
    }
}

/// The result of comparing two schemas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaDiff {
    /// Every breaking change, in a stable order.
    pub changes: Vec<SchemaChange>,
}

impl SchemaDiff {
    /// Whether the new schema is backwards compatible with the old one.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.changes.is_empty()
    }

    /// Iterate over breaking changes.
    pub fn changes(&self) -> impl Iterator<Item = &SchemaChange> {
        self.changes.iter()
    }
}

impl fmt::Display for SchemaDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, change) in self.changes.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            write!(f, "{change}")?;
        }
        Ok(())
    }
}

/// Compare two schemas and report changes that break existing operations.
///
/// Additive changes (new types, new optional arguments, new enum values, new optional input
/// fields) are compatible and deliberately not reported. This is the check a CI pipeline should
/// run before deploying a schema change.
#[must_use]
pub fn diff(old: &Schema, new: &Schema) -> SchemaDiff {
    let mut changes = Vec::new();

    for (name, old_ty) in &old.inner.types {
        if name.starts_with("__") {
            continue;
        }
        match new.get_type(name) {
            None => changes.push(SchemaChange::TypeRemoved {
                name: name.to_string(),
            }),
            Some(new_ty) if new_ty.kind() != old_ty.kind() => {
                changes.push(SchemaChange::TypeKindChanged {
                    name: name.to_string(),
                    from: old_ty.kind(),
                    to: new_ty.kind(),
                });
            }
            Some(new_ty) => diff_type(name, old_ty, new_ty, &mut changes),
        }
    }

    for (name, old_directive) in &old.inner.directives {
        match new.get_directive(name) {
            None => changes.push(SchemaChange::DirectiveRemoved {
                name: name.to_string(),
            }),
            Some(new_directive) => {
                for location in &old_directive.locations {
                    if !new_directive.locations.contains(location) {
                        changes.push(SchemaChange::DirectiveLocationRemoved {
                            name: name.to_string(),
                            location: location.as_str(),
                        });
                    }
                }
                for (argument, old_arg) in &old_directive.args {
                    match new_directive.args.get(argument) {
                        None => changes.push(SchemaChange::DirectiveArgumentRemoved {
                            directive: name.to_string(),
                            argument: argument.to_string(),
                        }),
                        Some(new_arg) => {
                            if !is_input_compatible(&old_arg.ty, &new_arg.ty) {
                                changes.push(SchemaChange::DirectiveArgumentTypeChanged {
                                    directive: name.to_string(),
                                    argument: argument.to_string(),
                                    from: old_arg.ty.clone(),
                                    to: new_arg.ty.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    SchemaDiff { changes }
}

fn diff_type(name: &str, old: &Type, new: &Type, changes: &mut Vec<SchemaChange>) {
    match (old, new) {
        (Type::Object(old), Type::Object(new)) => {
            diff_fields(name, &old.fields, &new.fields, changes);
            for interface in &old.interfaces {
                if !new.interfaces.contains(interface) {
                    changes.push(SchemaChange::InterfaceRemoved {
                        object_name: name.to_string(),
                        interface_name: interface.to_string(),
                    });
                }
            }
        }
        (Type::Interface(old), Type::Interface(new)) => {
            diff_fields(name, &old.fields, &new.fields, changes);
            for implemented in &old.interfaces {
                if !new.interfaces.contains(implemented) {
                    changes.push(SchemaChange::InterfaceImplementationRemoved {
                        interface_name: name.to_string(),
                        implemented: implemented.to_string(),
                    });
                }
            }
        }
        (Type::Enum(old), Type::Enum(new)) => {
            for value in old.values.keys() {
                if !new.values.contains_key(value) {
                    changes.push(SchemaChange::EnumValueRemoved {
                        enum_name: name.to_string(),
                        value: value.to_string(),
                    });
                }
            }
        }
        (Type::Union(old), Type::Union(new)) => {
            for member in &old.types {
                if !new.types.contains(member) {
                    changes.push(SchemaChange::UnionMemberRemoved {
                        union_name: name.to_string(),
                        member: member.to_string(),
                    });
                }
            }
        }
        (Type::InputObject(old), Type::InputObject(new)) => {
            for (field_name, old_field) in &old.fields {
                match new.fields.get(field_name) {
                    None => changes.push(SchemaChange::InputFieldRemoved {
                        input_name: name.to_string(),
                        field_name: field_name.to_string(),
                    }),
                    Some(new_field) => {
                        if !is_input_compatible(&old_field.ty, &new_field.ty) {
                            changes.push(SchemaChange::FieldTypeChanged {
                                type_name: name.to_string(),
                                field_name: field_name.to_string(),
                                from: old_field.ty.clone(),
                                to: new_field.ty.clone(),
                            });
                        }
                    }
                }
            }
            for (field_name, new_field) in &new.fields {
                let became_required = match old.fields.get(field_name) {
                    None => true,
                    Some(old_field) => {
                        !matches!(old_field.ty, TypeRef::NonNull(_))
                            && matches!(new_field.ty, TypeRef::NonNull(_))
                    }
                };
                if became_required
                    && matches!(new_field.ty, TypeRef::NonNull(_))
                    && new_field.default_value.is_none()
                {
                    changes.push(SchemaChange::InputFieldBecameRequired {
                        input_name: name.to_string(),
                        field_name: field_name.to_string(),
                    });
                }
            }
        }
        _ => {}
    }
}

fn diff_fields(
    type_name: &str,
    old_fields: &IndexMap<Name, Field>,
    new_fields: &IndexMap<Name, Field>,
    changes: &mut Vec<SchemaChange>,
) {
    for (field_name, old_field) in old_fields {
        match new_fields.get(field_name) {
            None => changes.push(SchemaChange::FieldRemoved {
                type_name: type_name.to_string(),
                field_name: field_name.to_string(),
            }),
            Some(new_field) => {
                if !is_output_compatible(&old_field.ty, &new_field.ty) {
                    changes.push(SchemaChange::FieldTypeChanged {
                        type_name: type_name.to_string(),
                        field_name: field_name.to_string(),
                        from: old_field.ty.clone(),
                        to: new_field.ty.clone(),
                    });
                }
                for (arg_name, old_arg) in &old_field.args {
                    match new_field.args.get(arg_name) {
                        None => changes.push(SchemaChange::ArgumentRemoved {
                            type_name: type_name.to_string(),
                            field_name: field_name.to_string(),
                            argument_name: arg_name.to_string(),
                        }),
                        Some(new_arg) => {
                            if !is_input_compatible(&old_arg.ty, &new_arg.ty) {
                                changes.push(SchemaChange::ArgumentTypeChanged {
                                    type_name: type_name.to_string(),
                                    field_name: field_name.to_string(),
                                    argument_name: arg_name.to_string(),
                                    from: old_arg.ty.clone(),
                                    to: new_arg.ty.clone(),
                                });
                            }
                        }
                    }
                }
                for (arg_name, new_arg) in &new_field.args {
                    if !old_field.args.contains_key(arg_name)
                        && matches!(new_arg.ty, TypeRef::NonNull(_))
                        && new_arg.default_value.is_none()
                    {
                        changes.push(SchemaChange::RequiredArgumentAdded {
                            type_name: type_name.to_string(),
                            field_name: field_name.to_string(),
                            argument_name: arg_name.to_string(),
                        });
                    }
                }
            }
        }
    }
}

fn is_output_compatible(old: &TypeRef, new: &TypeRef) -> bool {
    match (old, new) {
        (TypeRef::NonNull(old), TypeRef::NonNull(new)) => is_output_compatible(old, new),
        (TypeRef::NonNull(_), _) => false,
        (old, TypeRef::NonNull(new)) => is_output_compatible(old, new),
        (TypeRef::List(old), TypeRef::List(new)) => is_output_compatible(old, new),
        (TypeRef::List(_), _) | (_, TypeRef::List(_)) => false,
        (TypeRef::Named(old), TypeRef::Named(new)) => old == new,
    }
}

fn is_input_compatible(old: &TypeRef, new: &TypeRef) -> bool {
    match (old, new) {
        (TypeRef::NonNull(old), TypeRef::NonNull(new)) => is_input_compatible(old, new),
        (TypeRef::NonNull(old), new) => is_input_compatible(old, new),
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(old), TypeRef::List(new)) => is_input_compatible(old, new),
        (TypeRef::List(_), _) | (_, TypeRef::List(_)) => false,
        (TypeRef::Named(old), TypeRef::Named(new)) => old == new,
    }
}
