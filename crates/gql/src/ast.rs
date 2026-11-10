//! The GraphQL abstract syntax tree.
//!
//! The AST borrows names and string literals from the source document wherever possible. This
//! keeps parsing cheap: the only allocations are the vectors that hold nodes, and short escaped
//! strings (which become [`Cow::Owned`]).
//!
//! Both executable documents and the type system definition language (SDL) share this tree, so a
//! single parser can serve schema tooling, validation and execution.

use std::{borrow::Cow, fmt};

use crate::Location;

/// A parsed GraphQL document.
#[derive(Clone, Debug, PartialEq)]
pub struct Document<'a> {
    /// Definitions in source order.
    pub definitions: Vec<Definition<'a>>,
}

impl<'a> Document<'a> {
    /// Iterate over executable operations.
    pub fn operations(&self) -> impl Iterator<Item = &OperationDefinition<'a>> {
        self.definitions
            .iter()
            .filter_map(|definition| match definition {
                Definition::Operation(operation) => Some(operation),
                _ => None,
            })
    }

    /// Iterate over fragment definitions.
    pub fn fragments(&self) -> impl Iterator<Item = &FragmentDefinition<'a>> {
        self.definitions
            .iter()
            .filter_map(|definition| match definition {
                Definition::Fragment(fragment) => Some(fragment),
                _ => None,
            })
    }

    /// Look up a fragment definition by name.
    #[must_use]
    pub fn fragment(&self, name: &str) -> Option<&FragmentDefinition<'a>> {
        self.fragments().find(|fragment| fragment.name == name)
    }
}

/// A top-level definition.
#[derive(Clone, Debug, PartialEq)]
pub enum Definition<'a> {
    /// An executable operation.
    Operation(OperationDefinition<'a>),
    /// An executable fragment definition.
    Fragment(FragmentDefinition<'a>),
    /// A type system definition or extension.
    TypeSystem(TypeSystemDefinition<'a>),
}

/// A type system definition or extension.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeSystemDefinition<'a> {
    /// `schema { ... }`
    Schema(SchemaDefinition<'a>),
    /// A type definition.
    Type(TypeDefinition<'a>),
    /// `directive @... on ...`
    Directive(DirectiveDefinition<'a>),
    /// `extend schema { ... }`
    SchemaExtension(SchemaExtension<'a>),
    /// `extend type ...`
    TypeExtension(TypeExtension<'a>),
}

/// Whether an operation is a query, mutation or subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationKind {
    /// `query`
    Query,
    /// `mutation`
    Mutation,
    /// `subscription`
    Subscription,
}

impl OperationKind {
    /// The keyword used in a document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
            Self::Subscription => "subscription",
        }
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An executable operation definition.
#[derive(Clone, Debug, PartialEq)]
pub struct OperationDefinition<'a> {
    /// Query, mutation or subscription.
    pub kind: OperationKind,
    /// The operation name, absent for the anonymous shorthand.
    pub name: Option<&'a str>,
    /// Variable definitions.
    pub variables: Vec<VariableDefinition<'a>>,
    /// Directives applied to the operation.
    pub directives: Vec<Directive<'a>>,
    /// The root selection set.
    pub selection_set: SelectionSet<'a>,
    /// Where the operation starts.
    pub location: Location,
}

/// A variable definition.
#[derive(Clone, Debug, PartialEq)]
pub struct VariableDefinition<'a> {
    /// Variable name, without the `$`.
    pub name: &'a str,
    /// The declared type.
    pub ty: Type<'a>,
    /// An optional default value.
    pub default_value: Option<Value<'a>>,
    /// Directives applied to the variable definition.
    pub directives: Vec<Directive<'a>>,
    /// Where the variable definition starts.
    pub location: Location,
}

/// An executable fragment definition.
#[derive(Clone, Debug, PartialEq)]
pub struct FragmentDefinition<'a> {
    /// The fragment name.
    pub name: &'a str,
    /// The type condition following `on`.
    pub type_condition: &'a str,
    /// Directives applied to the fragment.
    pub directives: Vec<Directive<'a>>,
    /// The fragment's selection set.
    pub selection_set: SelectionSet<'a>,
    /// Where the fragment starts.
    pub location: Location,
}

/// A selection set.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionSet<'a> {
    /// Selections in source order.
    pub items: Vec<Selection<'a>>,
    /// Where the selection set starts.
    pub location: Location,
}

impl<'a> SelectionSet<'a> {
    /// Whether the selection set has no selections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// One selection within a selection set.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection<'a> {
    /// A field selection.
    Field(Field<'a>),
    /// `...FragmentName`
    FragmentSpread(FragmentSpread<'a>),
    /// `... on Type { ... }`
    InlineFragment(InlineFragment<'a>),
}

impl<'a> Selection<'a> {
    /// Whether this selection is affected by `@skip` or `@include` once variables are known.
    #[must_use]
    pub fn directives(&self) -> &[Directive<'a>] {
        match self {
            Self::Field(field) => &field.directives,
            Self::FragmentSpread(spread) => &spread.directives,
            Self::InlineFragment(fragment) => &fragment.directives,
        }
    }
}

/// A field selection.
#[derive(Clone, Debug, PartialEq)]
pub struct Field<'a> {
    /// The optional alias.
    pub alias: Option<&'a str>,
    /// The field name.
    pub name: &'a str,
    /// Arguments in source order.
    pub arguments: Vec<Argument<'a>>,
    /// Directives applied to the field.
    pub directives: Vec<Directive<'a>>,
    /// A nested selection set, when the field is composite.
    pub selection_set: Option<SelectionSet<'a>>,
    /// Where the field starts.
    pub location: Location,
}

impl<'a> Field<'a> {
    /// The key this field contributes to the response object.
    #[must_use]
    pub fn response_key(&self) -> &'a str {
        self.alias.unwrap_or(self.name)
    }

    /// Look up an argument by name.
    #[must_use]
    pub fn argument(&self, name: &str) -> Option<&Argument<'a>> {
        self.arguments.iter().find(|argument| argument.name == name)
    }
}

/// The field nodes a resolver is responsible for.
///
/// A field can appear several times in one selection set once fragments are expanded, and the
/// specification merges those nodes into one response key. `FieldNodes` exposes every node behind
/// the field currently being resolved, in document order.
///
/// The nodes are always borrowed; execution lends them for the resolver's lifetime and no
/// ownership crosses that boundary.
#[derive(Clone, Copy, Debug)]
pub struct FieldNodes<'a> {
    nodes: &'a [&'a Field<'a>],
}

impl<'a> FieldNodes<'a> {
    /// No field nodes.
    #[must_use]
    pub const fn empty() -> Self {
        Self { nodes: &[] }
    }

    /// Field nodes borrowed from the execution.
    #[must_use]
    pub(crate) const fn borrowed(nodes: &'a [&'a Field<'a>]) -> Self {
        Self { nodes }
    }

    /// The nodes as one borrowed slice.
    #[must_use]
    pub const fn as_slice(&self) -> &'a [&'a Field<'a>] {
        self.nodes
    }

    /// The number of merged field nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there are no field nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The first field node.
    #[must_use]
    pub fn first(&self) -> Option<&'a Field<'a>> {
        self.nodes.first().copied()
    }

    /// The node at `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&'a Field<'a>> {
        self.nodes.get(index).copied()
    }

    /// Iterate over the merged field nodes in document order.
    pub fn iter(&self) -> FieldNodesIter<'a> {
        self.nodes.iter().copied()
    }
}

impl<'a> IntoIterator for FieldNodes<'a> {
    type Item = &'a Field<'a>;
    type IntoIter = FieldNodesIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> std::ops::Deref for FieldNodes<'a> {
    type Target = [&'a Field<'a>];

    fn deref(&self) -> &Self::Target {
        self.nodes
    }
}

impl<'a> AsRef<[&'a Field<'a>]> for FieldNodes<'a> {
    fn as_ref(&self) -> &[&'a Field<'a>] {
        self.nodes
    }
}

/// An iterator over the merged nodes of a [`FieldNodes`].
pub type FieldNodesIter<'a> = std::iter::Copied<std::slice::Iter<'a, &'a Field<'a>>>;

/// A named fragment spread.
#[derive(Clone, Debug, PartialEq)]
pub struct FragmentSpread<'a> {
    /// The fragment name.
    pub name: &'a str,
    /// Directives applied to the spread.
    pub directives: Vec<Directive<'a>>,
    /// Where the spread starts.
    pub location: Location,
}

/// An inline fragment.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineFragment<'a> {
    /// The optional type condition.
    pub type_condition: Option<&'a str>,
    /// Directives applied to the fragment.
    pub directives: Vec<Directive<'a>>,
    /// The fragment's selection set.
    pub selection_set: SelectionSet<'a>,
    /// Where the fragment starts.
    pub location: Location,
}

/// A field argument.
#[derive(Clone, Debug, PartialEq)]
pub struct Argument<'a> {
    /// The argument name.
    pub name: &'a str,
    /// The argument value.
    pub value: Value<'a>,
    /// Where the argument starts.
    pub location: Location,
}

/// A directive application.
#[derive(Clone, Debug, PartialEq)]
pub struct Directive<'a> {
    /// The directive name without `@`.
    pub name: &'a str,
    /// Arguments in source order.
    pub arguments: Vec<Argument<'a>>,
    /// Where the directive starts.
    pub location: Location,
}

impl<'a> Directive<'a> {
    /// Look up an argument by name.
    #[must_use]
    pub fn argument(&self, name: &str) -> Option<&Argument<'a>> {
        self.arguments.iter().find(|argument| argument.name == name)
    }
}

/// A GraphQL literal value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value<'a> {
    /// A variable reference, without the `$`.
    Variable(&'a str),
    /// An integer literal. Parsed as `i128` so validation can report out-of-range literals for
    /// both the 32-bit `Int` scalar and custom scalars that accept larger integers.
    Int(i128),
    /// A float literal.
    Float(f64),
    /// A string or block string.
    String(Cow<'a, str>),
    /// A boolean literal.
    Boolean(bool),
    /// The `null` literal.
    Null,
    /// An enum literal.
    Enum(&'a str),
    /// A list literal.
    List(Vec<Value<'a>>),
    /// An object literal.
    Object(Vec<ObjectField<'a>>),
}

impl<'a> Value<'a> {
    /// Look up an object field by name, when the value is an object.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value<'a>> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|field| field.name == name)
                .map(|field| &field.value),
            _ => None,
        }
    }

    /// Whether the value is a variable reference.
    #[must_use]
    pub const fn is_variable(&self) -> bool {
        matches!(self, Self::Variable(_))
    }
}

/// One field of an object literal.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectField<'a> {
    /// The field name.
    pub name: &'a str,
    /// The field value.
    pub value: Value<'a>,
    /// Where the field starts.
    pub location: Location,
}

/// A type reference, e.g. `[User!]!`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type<'a> {
    /// A named type.
    Named(&'a str),
    /// A list type.
    List(Box<Type<'a>>),
    /// A non-null type.
    NonNull(Box<Type<'a>>),
}

impl<'a> Type<'a> {
    /// The innermost named type.
    #[must_use]
    pub fn named_type(&self) -> &'a str {
        match self {
            Self::Named(name) => name,
            Self::List(inner) | Self::NonNull(inner) => inner.named_type(),
        }
    }

    /// Whether the outermost type is non-null.
    #[must_use]
    pub const fn is_non_null(&self) -> bool {
        matches!(self, Self::NonNull(_))
    }
}

impl fmt::Display for Type<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => f.write_str(name),
            Self::List(inner) => write!(f, "[{inner}]"),
            Self::NonNull(inner) => write!(f, "{inner}!"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Type system definition language
// ---------------------------------------------------------------------------------------------

/// A `schema { ... }` definition.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDefinition<'a> {
    /// Optional description.
    pub description: Option<Cow<'a, str>>,
    /// Directives applied to the schema.
    pub directives: Vec<Directive<'a>>,
    /// Explicit root operation types.
    pub operations: Vec<RootOperationTypeDefinition<'a>>,
    /// Where the definition starts.
    pub location: Location,
}

/// One root operation type mapping.
#[derive(Clone, Debug, PartialEq)]
pub struct RootOperationTypeDefinition<'a> {
    /// Which operation this is the root for.
    pub operation: OperationKind,
    /// The root type name.
    pub ty: &'a str,
    /// Where the mapping starts.
    pub location: Location,
}

/// A type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDefinition<'a> {
    /// Optional description.
    pub description: Option<Cow<'a, str>>,
    /// The type name.
    pub name: &'a str,
    /// The actual definition.
    pub kind: TypeDefinitionKind<'a>,
    /// Directives applied to the type.
    pub directives: Vec<Directive<'a>>,
    /// Where the definition starts.
    pub location: Location,
}

/// The kind-specific body of a type definition.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeDefinitionKind<'a> {
    /// `scalar Name`
    Scalar,
    /// `type Name implements A & B { ... }`
    Object(ObjectTypeDefinition<'a>),
    /// `interface Name implements A { ... }`
    Interface(InterfaceTypeDefinition<'a>),
    /// `union Name = A | B`
    Union(UnionTypeDefinition<'a>),
    /// `enum Name { A B }`
    Enum(EnumTypeDefinition<'a>),
    /// `input Name { ... }`
    InputObject(InputObjectTypeDefinition<'a>),
}

/// An object type definition body.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectTypeDefinition<'a> {
    /// Interfaces implemented by the object.
    pub interfaces: Vec<&'a str>,
    /// The object's fields.
    pub fields: Vec<FieldDefinition<'a>>,
}

/// An interface type definition body.
#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceTypeDefinition<'a> {
    /// Interfaces implemented by this interface.
    pub interfaces: Vec<&'a str>,
    /// The interface's fields.
    pub fields: Vec<FieldDefinition<'a>>,
}

/// A union type definition body.
#[derive(Clone, Debug, PartialEq)]
pub struct UnionTypeDefinition<'a> {
    /// Member object type names.
    pub types: Vec<&'a str>,
}

/// An enum type definition body.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumTypeDefinition<'a> {
    /// The enum's values.
    pub values: Vec<EnumValueDefinition<'a>>,
}

/// An input object type definition body.
#[derive(Clone, Debug, PartialEq)]
pub struct InputObjectTypeDefinition<'a> {
    /// The input object's fields.
    pub fields: Vec<InputValueDefinition<'a>>,
}

/// A field definition on an object or interface.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldDefinition<'a> {
    /// Optional description.
    pub description: Option<Cow<'a, str>>,
    /// The field name.
    pub name: &'a str,
    /// Arguments accepted by the field.
    pub arguments: Vec<InputValueDefinition<'a>>,
    /// The field's output type.
    pub ty: Type<'a>,
    /// Directives applied to the field.
    pub directives: Vec<Directive<'a>>,
    /// Where the field starts.
    pub location: Location,
}

/// An input value definition: a field argument or input object field.
#[derive(Clone, Debug, PartialEq)]
pub struct InputValueDefinition<'a> {
    /// Optional description.
    pub description: Option<Cow<'a, str>>,
    /// The input value name.
    pub name: &'a str,
    /// The input value's type.
    pub ty: Type<'a>,
    /// An optional default value.
    pub default_value: Option<Value<'a>>,
    /// Directives applied to the input value.
    pub directives: Vec<Directive<'a>>,
    /// Where the definition starts.
    pub location: Location,
}

/// An enum value definition.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumValueDefinition<'a> {
    /// Optional description.
    pub description: Option<Cow<'a, str>>,
    /// The value name.
    pub name: &'a str,
    /// Directives applied to the enum value.
    pub directives: Vec<Directive<'a>>,
    /// Where the value starts.
    pub location: Location,
}

/// A directive definition.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectiveDefinition<'a> {
    /// Optional description.
    pub description: Option<Cow<'a, str>>,
    /// The directive name without `@`.
    pub name: &'a str,
    /// Arguments accepted by the directive.
    pub arguments: Vec<InputValueDefinition<'a>>,
    /// Whether the directive may appear multiple times in one location.
    pub repeatable: bool,
    /// Directives applied to the definition itself.
    pub directives: Vec<Directive<'a>>,
    /// Locations the directive may appear in.
    pub locations: Vec<DirectiveLocation>,
    /// Where the definition starts.
    pub location: Location,
}

/// A schema extension.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaExtension<'a> {
    /// Directives applied.
    pub directives: Vec<Directive<'a>>,
    /// Additional root operation types.
    pub operations: Vec<RootOperationTypeDefinition<'a>>,
    /// Where the extension starts.
    pub location: Location,
}

/// A type system extension.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeExtension<'a> {
    /// The extended type's name.
    pub name: &'a str,
    /// The kind-specific additions.
    pub kind: TypeExtensionKind<'a>,
    /// Directives applied.
    pub directives: Vec<Directive<'a>>,
    /// Where the extension starts.
    pub location: Location,
}

/// The kind-specific body of a type extension.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExtensionKind<'a> {
    /// `extend scalar Name @directive`
    Scalar,
    /// `extend type Name implements A { ... }`
    Object(ObjectTypeDefinition<'a>),
    /// `extend interface Name implements A { ... }`
    Interface(InterfaceTypeDefinition<'a>),
    /// `extend union Name = A | B`
    Union(UnionTypeDefinition<'a>),
    /// `extend enum Name { A B }`
    Enum(EnumTypeDefinition<'a>),
    /// `extend input Name { ... }`
    InputObject(InputObjectTypeDefinition<'a>),
}

/// A directive location, matching the specification's `DirectiveLocation` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DirectiveLocation {
    /// `QUERY`
    Query,
    /// `MUTATION`
    Mutation,
    /// `SUBSCRIPTION`
    Subscription,
    /// `FIELD`
    Field,
    /// `FRAGMENT_DEFINITION`
    FragmentDefinition,
    /// `FRAGMENT_SPREAD`
    FragmentSpread,
    /// `INLINE_FRAGMENT`
    InlineFragment,
    /// `VARIABLE_DEFINITION`
    VariableDefinition,
    /// `SCHEMA`
    Schema,
    /// `SCALAR`
    Scalar,
    /// `OBJECT`
    Object,
    /// `FIELD_DEFINITION`
    FieldDefinition,
    /// `DIRECTIVE_DEFINITION`
    DirectiveDefinition,
    /// `ARGUMENT_DEFINITION`
    ArgumentDefinition,
    /// `INTERFACE`
    Interface,
    /// `UNION`
    Union,
    /// `ENUM`
    Enum,
    /// `ENUM_VALUE`
    EnumValue,
    /// `INPUT_OBJECT`
    InputObject,
    /// `INPUT_FIELD_DEFINITION`
    InputFieldDefinition,
}

impl DirectiveLocation {
    /// The specification name of the location.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "QUERY",
            Self::Mutation => "MUTATION",
            Self::Subscription => "SUBSCRIPTION",
            Self::Field => "FIELD",
            Self::FragmentDefinition => "FRAGMENT_DEFINITION",
            Self::FragmentSpread => "FRAGMENT_SPREAD",
            Self::InlineFragment => "INLINE_FRAGMENT",
            Self::VariableDefinition => "VARIABLE_DEFINITION",
            Self::Schema => "SCHEMA",
            Self::Scalar => "SCALAR",
            Self::Object => "OBJECT",
            Self::FieldDefinition => "FIELD_DEFINITION",
            Self::DirectiveDefinition => "DIRECTIVE_DEFINITION",
            Self::ArgumentDefinition => "ARGUMENT_DEFINITION",
            Self::Interface => "INTERFACE",
            Self::Union => "UNION",
            Self::Enum => "ENUM",
            Self::EnumValue => "ENUM_VALUE",
            Self::InputObject => "INPUT_OBJECT",
            Self::InputFieldDefinition => "INPUT_FIELD_DEFINITION",
        }
    }

    /// Parse a location by its specification name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "QUERY" => Self::Query,
            "MUTATION" => Self::Mutation,
            "SUBSCRIPTION" => Self::Subscription,
            "FIELD" => Self::Field,
            "FRAGMENT_DEFINITION" => Self::FragmentDefinition,
            "FRAGMENT_SPREAD" => Self::FragmentSpread,
            "INLINE_FRAGMENT" => Self::InlineFragment,
            "VARIABLE_DEFINITION" => Self::VariableDefinition,
            "SCHEMA" => Self::Schema,
            "SCALAR" => Self::Scalar,
            "OBJECT" => Self::Object,
            "FIELD_DEFINITION" => Self::FieldDefinition,
            "DIRECTIVE_DEFINITION" => Self::DirectiveDefinition,
            "ARGUMENT_DEFINITION" => Self::ArgumentDefinition,
            "INTERFACE" => Self::Interface,
            "UNION" => Self::Union,
            "ENUM" => Self::Enum,
            "ENUM_VALUE" => Self::EnumValue,
            "INPUT_OBJECT" => Self::InputObject,
            "INPUT_FIELD_DEFINITION" => Self::InputFieldDefinition,
            _ => return None,
        })
    }
}
