//! Schema definition language output.
//!
//! [`print_schema`] renders a built [`Schema`] as a GraphQL SDL document that is valid input for
//! the sibling parser. The output is stable: the schema definition comes first, then custom
//! directives, then every type in schema definition order. Built-in scalars, built-in directives
//! and the introspection types are omitted because a client already knows them.

use std::fmt::Write as _;

use indexmap::IndexMap;

use crate::{
    Value,
    schema::Schema,
    types::{
        AppliedDirective, DirectiveDefinition, EnumType, Field, InputObjectType, InputValue,
        InterfaceType, Name, ObjectType, ScalarType, Type, UnionType,
    },
};

/// The scalars every GraphQL schema implicitly defines.
const BUILT_IN_SCALARS: [&str; 5] = ["Int", "Float", "String", "Boolean", "ID"];

/// The directives every GraphQL schema implicitly defines.
const BUILT_IN_DIRECTIVES: [&str; 5] = ["skip", "include", "deprecated", "specifiedBy", "oneOf"];

/// The reason the specification assigns when none is supplied.
const DEFAULT_DEPRECATION_REASON: &str = "No longer supported";

/// Directives whose authoritative data lives in a dedicated model field.
///
/// They are still recorded in [`AppliedDirective`] lists, but printing them from there as well
/// would duplicate the `deprecation_reason`, `specified_by_url` and `one_of` fields.
const DEDICATED_DIRECTIVES: [&str; 3] = ["deprecated", "specifiedBy", "oneOf"];

/// Render a schema as GraphQL SDL.
#[must_use]
pub fn print_schema(schema: &Schema) -> String {
    let mut output = String::new();
    let mut wrote = false;

    if write_schema_definition(schema, &mut output) {
        wrote = true;
    }

    for (name, directive) in schema.directives() {
        if BUILT_IN_DIRECTIVES.contains(&name.as_ref()) {
            continue;
        }
        push_separator(&mut output, &mut wrote);
        write_directive(&mut output, directive);
    }

    for (name, ty) in schema.types() {
        if name.starts_with("__") {
            continue;
        }
        // The engine adds the built-in scalars only when a field references them, and a client
        // already knows their coercion rules. A scalar the user registered explicitly (even one
        // named `Int`) has its own behaviour, so it must be printed and parsed back rather than
        // silently replaced by the built-in definition.
        if BUILT_IN_SCALARS.contains(&name.as_ref())
            && matches!(ty, Type::Scalar(scalar) if scalar.builtin)
        {
            continue;
        }
        push_separator(&mut output, &mut wrote);
        write_type(&mut output, ty);
    }

    if wrote {
        output.push('\n');
    }
    output
}

/// Write the blank line between two definitions.
fn push_separator(output: &mut String, wrote: &mut bool) {
    if *wrote {
        output.push_str("\n\n");
    }
    *wrote = true;
}

/// Print a runtime value using GraphQL literal syntax.
///
/// This is used for default values in SDL and in introspection. Values that cannot occur in a
/// GraphQL document (an opaque [`Value::External`] or a non-finite float) print as `null` so the
/// output always re-parses.
pub(crate) fn print_value(value: &Value) -> String {
    let mut output = String::new();
    write_value(&mut output, value);
    output
}

fn write_value(output: &mut String, value: &Value) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Boolean(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Int(value) => {
            let _ = write!(output, "{value}");
        }
        Value::BigInt(value) => {
            let _ = write!(output, "{value}");
        }
        Value::Float(value) => {
            if value.is_finite() {
                let printed = value.to_string();
                // A GraphQL Int literal is also a valid Float literal, but printing `1.0` makes
                // the intent unambiguous and keeps round-tripping stable.
                if printed.contains(['.', 'e', 'E']) {
                    output.push_str(&printed);
                } else {
                    output.push_str(&printed);
                    output.push_str(".0");
                }
            } else {
                output.push_str("null");
            }
        }
        Value::String(value) => write_string(output, value),
        Value::Enum(value) => output.push_str(value),
        Value::List(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push_str(", ");
                }
                write_value(output, value);
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            for (index, (key, value)) in object.iter().enumerate() {
                if index != 0 {
                    output.push_str(", ");
                }
                output.push_str(key);
                output.push_str(": ");
                write_value(output, value);
            }
            output.push('}');
        }
        Value::External(_) => output.push_str("null"),
    }
}

fn write_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000C}' => output.push_str("\\f"),
            character if (character as u32) < 0x20 => {
                let _ = write!(output, "\\u{:04X}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

/// Write the `schema { ... }` definition when the defaults do not describe the schema.
///
/// Returns whether anything was written.
fn write_schema_definition(schema: &Schema, output: &mut String) -> bool {
    let has_description = schema.description().is_some();
    let has_directives = !schema.applied_directives().is_empty();
    let custom_query = schema.query_name().as_ref() != "Query";
    let custom_mutation = schema
        .mutation_name()
        .is_some_and(|name| name.as_ref() != "Mutation");
    let custom_subscription = schema
        .subscription_name()
        .is_some_and(|name| name.as_ref() != "Subscription");
    // A non-root type that happens to be named `Mutation` or `Subscription` is inferred as a root
    // by an SDL reader when the document has no schema definition. Printing an explicit
    // definition is what records that the type is not a root.
    let shadowed_mutation =
        schema.mutation_name().is_none() && schema.get_type("Mutation").is_some();
    let shadowed_subscription =
        schema.subscription_name().is_none() && schema.get_type("Subscription").is_some();

    if !(has_description
        || has_directives
        || custom_query
        || custom_mutation
        || custom_subscription
        || shadowed_mutation
        || shadowed_subscription)
    {
        return false;
    }

    push_description(output, schema.description(), "");
    output.push_str("schema");
    push_applied_directives(output, schema.applied_directives());
    output.push_str(" {\n");
    let _ = writeln!(output, "  query: {}", schema.query_name());
    if let Some(name) = schema.mutation_name() {
        let _ = writeln!(output, "  mutation: {name}");
    }
    if let Some(name) = schema.subscription_name() {
        let _ = writeln!(output, "  subscription: {name}");
    }
    output.push('}');
    true
}

fn write_type(output: &mut String, ty: &Type) {
    match ty {
        Type::Scalar(scalar) => write_scalar(output, scalar),
        Type::Object(object) => write_object(output, object),
        Type::Interface(interface) => write_interface(output, interface),
        Type::Union(union) => write_union(output, union),
        Type::Enum(enumeration) => write_enum(output, enumeration),
        Type::InputObject(input) => write_input_object(output, input),
    }
}

fn write_scalar(output: &mut String, scalar: &ScalarType) {
    push_description(output, scalar.description.as_deref(), "");
    let _ = write!(output, "scalar {}", scalar.name);
    if let Some(url) = &scalar.specified_by_url {
        output.push_str(" @specifiedBy(url: ");
        write_string(output, url);
        output.push(')');
    }
    push_applied_directives(output, &scalar.directives);
}

fn write_object(output: &mut String, object: &ObjectType) {
    push_description(output, object.description.as_deref(), "");
    let _ = write!(output, "type {}", object.name);
    push_implements(output, &object.interfaces);
    push_applied_directives(output, &object.directives);
    push_fields(output, &object.fields);
}

fn write_interface(output: &mut String, interface: &InterfaceType) {
    push_description(output, interface.description.as_deref(), "");
    let _ = write!(output, "interface {}", interface.name);
    push_implements(output, &interface.interfaces);
    push_applied_directives(output, &interface.directives);
    push_fields(output, &interface.fields);
}

fn write_union(output: &mut String, union: &UnionType) {
    push_description(output, union.description.as_deref(), "");
    let _ = write!(output, "union {}", union.name);
    push_applied_directives(output, &union.directives);
    if !union.types.is_empty() {
        output.push_str(" = ");
        for (index, name) in union.types.iter().enumerate() {
            if index != 0 {
                output.push_str(" | ");
            }
            output.push_str(name);
        }
    }
}

fn write_enum(output: &mut String, enumeration: &EnumType) {
    push_description(output, enumeration.description.as_deref(), "");
    let _ = write!(output, "enum {}", enumeration.name);
    push_applied_directives(output, &enumeration.directives);
    if enumeration.values.is_empty() {
        return;
    }
    output.push_str(" {\n");
    for value in enumeration.values.values() {
        push_description(output, value.description.as_deref(), "  ");
        let _ = write!(output, "  {}", value.name);
        push_deprecation(output, value.deprecation_reason.as_deref());
        push_applied_directives(output, &value.directives);
        output.push('\n');
    }
    output.push('}');
}

fn write_input_object(output: &mut String, input: &InputObjectType) {
    push_description(output, input.description.as_deref(), "");
    let _ = write!(output, "input {}", input.name);
    if input.one_of {
        output.push_str(" @oneOf");
    }
    push_applied_directives(output, &input.directives);
    if input.fields.is_empty() {
        return;
    }
    output.push_str(" {\n");
    for field in input.fields.values() {
        push_description(output, field.description.as_deref(), "  ");
        output.push_str("  ");
        write_input_value(output, field);
        output.push('\n');
    }
    output.push('}');
}

fn write_directive(output: &mut String, directive: &DirectiveDefinition) {
    push_description(output, directive.description.as_deref(), "");
    let _ = write!(output, "directive @{}", directive.name);
    write_arguments(output, &directive.args, "");
    if directive.repeatable {
        output.push_str(" repeatable");
    }
    push_deprecation(output, directive.deprecation_reason.as_deref());
    push_applied_directives(output, &directive.directives);
    output.push_str(" on ");
    for (index, location) in directive.locations.iter().enumerate() {
        if index != 0 {
            output.push_str(" | ");
        }
        output.push_str(location.as_str());
    }
}

fn push_implements(output: &mut String, interfaces: &[Name]) {
    if interfaces.is_empty() {
        return;
    }
    output.push_str(" implements ");
    for (index, name) in interfaces.iter().enumerate() {
        if index != 0 {
            output.push_str(" & ");
        }
        output.push_str(name);
    }
}

fn push_fields(output: &mut String, fields: &IndexMap<Name, Field>) {
    if fields.is_empty() {
        return;
    }
    output.push_str(" {\n");
    for field in fields.values() {
        write_field(output, field, "  ");
        output.push('\n');
    }
    output.push('}');
}

fn write_field(output: &mut String, field: &Field, indent: &str) {
    push_description(output, field.description.as_deref(), indent);
    output.push_str(indent);
    output.push_str(&field.name);
    write_arguments(output, &field.args, indent);
    let _ = write!(output, ": {}", field.ty);
    push_deprecation(output, field.deprecation_reason.as_deref());
    push_applied_directives(output, &field.directives);
}

fn write_arguments(output: &mut String, args: &IndexMap<Name, InputValue>, indent: &str) {
    if args.is_empty() {
        return;
    }

    let multiline = args.values().any(|argument| {
        argument
            .description
            .as_deref()
            .is_some_and(|description| !description.is_empty())
            || argument.deprecation_reason.is_some()
            || !argument.directives.is_empty()
    });

    if !multiline {
        output.push('(');
        for (index, argument) in args.values().enumerate() {
            if index != 0 {
                output.push_str(", ");
            }
            write_input_value(output, argument);
        }
        output.push(')');
        return;
    }

    let mut inner = String::with_capacity(indent.len() + 2);
    inner.push_str(indent);
    inner.push_str("  ");
    output.push_str("(\n");
    for (index, argument) in args.values().enumerate() {
        if index != 0 {
            output.push('\n');
        }
        push_description(output, argument.description.as_deref(), &inner);
        output.push_str(&inner);
        write_input_value(output, argument);
    }
    output.push('\n');
    output.push_str(indent);
    output.push(')');
}

fn write_input_value(output: &mut String, value: &InputValue) {
    let _ = write!(output, "{}: {}", value.name, value.ty);
    if let Some(default) = &value.default_value {
        output.push_str(" = ");
        write_value(output, default);
    }
    push_deprecation(output, value.deprecation_reason.as_deref());
    push_applied_directives(output, &value.directives);
}

fn push_deprecation(output: &mut String, reason: Option<&str>) {
    let Some(reason) = reason else { return };
    if reason == DEFAULT_DEPRECATION_REASON {
        output.push_str(" @deprecated");
    } else {
        output.push_str(" @deprecated(reason: ");
        write_string(output, reason);
        output.push(')');
    }
}

/// Print every applied directive that does not have a dedicated model field.
fn push_applied_directives(output: &mut String, directives: &[AppliedDirective]) {
    for directive in directives {
        if DEDICATED_DIRECTIVES.contains(&directive.name.as_ref()) {
            continue;
        }
        output.push_str(" @");
        output.push_str(&directive.name);
        if directive.arguments.is_empty() {
            continue;
        }
        output.push('(');
        for (index, (name, value)) in directive.arguments.iter().enumerate() {
            if index != 0 {
                output.push_str(", ");
            }
            output.push_str(name);
            output.push_str(": ");
            write_value(output, value);
        }
        output.push(')');
    }
}

/// Write a description as a block string above its definition.
fn push_description(output: &mut String, description: Option<&str>, indent: &str) {
    let Some(description) = description else {
        return;
    };
    if description.is_empty() {
        return;
    }

    // Block strings normalise every line ending to `\n`, so a description containing a carriage
    // return would not survive a print/parse round-trip. Print those as a normal (escaped)
    // string instead, where `\r` has an explicit escape.
    if description.contains('\r') {
        output.push_str(indent);
        output.push_str(&print_value(&Value::from(description)));
        output.push('\n');
        return;
    }

    // Block strings only recognise `\"""` as an escape, so a trailing quote or backslash directly
    // before the closing delimiter would swallow it. Printing such a description on multiple
    // lines puts a line break in front of the delimiter instead.
    let escaped = description.replace("\"\"\"", "\\\"\"\"");
    let multiline = escaped.contains('\n') || escaped.ends_with('"') || escaped.ends_with('\\');

    if !multiline {
        output.push_str(indent);
        output.push_str("\"\"\"");
        output.push_str(&escaped);
        output.push_str("\"\"\"\n");
        return;
    }

    output.push_str(indent);
    output.push_str("\"\"\"\n");
    for line in escaped.split('\n') {
        if !line.is_empty() {
            output.push_str(indent);
            output.push_str(line);
        }
        output.push('\n');
    }
    output.push_str(indent);
    output.push_str("\"\"\"\n");
}
