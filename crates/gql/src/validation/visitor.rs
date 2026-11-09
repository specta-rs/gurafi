//! Type-aware traversal of executable selection sets.
//!
//! Every validation rule that needs to know the type of a field walks the document through this
//! module. The walker resolves field definitions (including the specification's meta-fields) and
//! keeps track of the parent type of each selection set.

use std::sync::OnceLock;

use crate::{
    ast::{self, Field, InlineFragment, Selection, SelectionSet},
    schema::Schema,
    types::{Field as FieldDefinition, Type, TypeRef},
    validation::ValidationOptions,
};

/// A meta-field defined by the specification rather than by the schema's type map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetaField {
    /// `__typename`, available on every composite type.
    Typename,
    /// `__schema`, available on the query root when introspection is enabled.
    Schema,
    /// `__type`, available on the query root when introspection is enabled.
    Type,
}

/// The schema context of one field selection.
#[derive(Clone, Debug)]
pub(crate) struct FieldContext<'a> {
    /// The type the field is selected on, when it could be resolved.
    pub(crate) parent_type: Option<&'a Type>,
    /// The field's definition, when it is a normal schema field.
    pub(crate) definition: Option<&'a FieldDefinition>,
    /// The meta-field this selection refers to, when it is one.
    pub(crate) meta: Option<MetaField>,
    /// The field's declared return type. `None` when the field is unknown.
    pub(crate) return_type: Option<&'a TypeRef>,
}

/// One argument's input type and whether it has a default value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ArgumentInfo<'a> {
    /// The expected input type.
    pub(crate) ty: &'a TypeRef,
    /// Whether the argument defines a default value.
    pub(crate) has_default: bool,
}

impl<'a> ArgumentInfo<'a> {
    fn from_input(input: &'a crate::types::InputValue) -> Self {
        Self {
            ty: &input.ty,
            has_default: input.default_value.is_some(),
        }
    }
}

/// The return type of a specification meta-field.
fn meta_type(meta: MetaField) -> &'static TypeRef {
    static TYPENAME: OnceLock<TypeRef> = OnceLock::new();
    static SCHEMA: OnceLock<TypeRef> = OnceLock::new();
    static TYPE: OnceLock<TypeRef> = OnceLock::new();
    match meta {
        MetaField::Typename => TYPENAME.get_or_init(|| TypeRef::string().non_null()),
        MetaField::Schema => SCHEMA.get_or_init(|| TypeRef::named("__Schema").non_null()),
        MetaField::Type => TYPE.get_or_init(|| TypeRef::named("__Type")),
    }
}

/// Convert an AST type reference into the schema's runtime representation.
pub(crate) fn type_ref_from_ast(ty: &ast::Type<'_>) -> TypeRef {
    match ty {
        ast::Type::Named(name) => TypeRef::named(*name),
        ast::Type::List(inner) => TypeRef::List(Box::new(type_ref_from_ast(inner))),
        ast::Type::NonNull(inner) => TypeRef::NonNull(Box::new(type_ref_from_ast(inner))),
    }
}

/// Resolve the meta-field named `name` on `parent`, if it is available.
pub(crate) fn meta_field(
    schema: &Schema,
    options: &ValidationOptions,
    parent: Option<&Type>,
    name: &str,
) -> Option<MetaField> {
    let parent = parent?;
    match name {
        "__typename" if parent.is_composite() => Some(MetaField::Typename),
        "__schema" | "__type"
            if options.introspection
                && parent.is_composite()
                && parent.name() == schema.query_name() =>
        {
            Some(if name == "__schema" {
                MetaField::Schema
            } else {
                MetaField::Type
            })
        }
        _ => None,
    }
}

/// Compute the schema context of a field selection.
pub(crate) fn field_context<'a>(
    schema: &'a Schema,
    options: &ValidationOptions,
    parent: Option<&'a Type>,
    field: &'a Field<'a>,
) -> FieldContext<'a> {
    let meta = meta_field(schema, options, parent, field.name);
    let definition = if meta.is_some() {
        None
    } else {
        parent.and_then(|parent| match parent {
            Type::Object(object) => object.fields.get(field.name),
            Type::Interface(interface) => interface.fields.get(field.name),
            _ => None,
        })
    };
    let return_type = match meta {
        Some(meta) => Some(meta_type(meta)),
        None => definition.map(|definition| &definition.ty),
    };

    FieldContext {
        parent_type: parent,
        definition,
        meta,
        return_type,
    }
}

/// The input type of a field argument, including the synthetic `__type(name:)` argument.
pub(crate) fn field_argument_info<'a>(
    context: &FieldContext<'a>,
    name: &str,
) -> Option<ArgumentInfo<'a>> {
    if let Some(definition) = context.definition {
        return definition.args.get(name).map(ArgumentInfo::from_input);
    }
    match context.meta {
        Some(MetaField::Type) if name == "name" => Some(ArgumentInfo {
            ty: meta_type(MetaField::Typename),
            has_default: false,
        }),
        _ => None,
    }
}

/// The input type of a directive argument.
pub(crate) fn directive_argument_info<'a>(
    definition: &'a crate::types::DirectiveDefinition,
    name: &str,
) -> Option<ArgumentInfo<'a>> {
    definition.args.get(name).map(ArgumentInfo::from_input)
}

/// Callbacks invoked while walking a selection set.
pub(crate) trait Visitor<'a> {
    /// Visit one field selection.
    fn field(&mut self, field: &'a Field<'a>, context: &FieldContext<'a>) {
        let _ = (field, context);
    }

    /// Visit an inline fragment.
    fn inline_fragment(&mut self, fragment: &'a InlineFragment<'a>, parent: Option<&'a Type>) {
        let _ = (fragment, parent);
    }

    /// Visit a named fragment spread. The fragment's own selection set is visited separately,
    /// when the fragment definition itself is validated.
    fn fragment_spread(&mut self, spread: &'a ast::FragmentSpread<'a>, parent: Option<&'a Type>) {
        let _ = (spread, parent);
    }
}

/// Walk a selection set, tracking the parent type of every field.
pub(crate) fn walk<'a, V: Visitor<'a>>(
    schema: &'a Schema,
    options: &ValidationOptions,
    parent: Option<&'a Type>,
    selection_set: &'a SelectionSet<'a>,
    visitor: &mut V,
) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                let context = field_context(schema, options, parent, field);
                visitor.field(field, &context);
                if let Some(child_set) = &field.selection_set {
                    // Descending into a non-composite type produces a cascade of errors that
                    // ScalarLeafs has already reported, so stop tracking the type there.
                    let child = context
                        .return_type
                        .as_ref()
                        .and_then(|ty| schema.get_type(ty.named_type()))
                        .filter(|ty| ty.is_composite());
                    walk(schema, options, child, child_set, visitor);
                }
            }
            Selection::InlineFragment(fragment) => {
                visitor.inline_fragment(fragment, parent);
                let condition = fragment
                    .type_condition
                    .and_then(|name| schema.get_type(name))
                    .filter(|ty| ty.is_composite())
                    .or(parent);
                walk(schema, options, condition, &fragment.selection_set, visitor);
            }
            Selection::FragmentSpread(spread) => {
                visitor.fragment_spread(spread, parent);
            }
        }
    }
}
