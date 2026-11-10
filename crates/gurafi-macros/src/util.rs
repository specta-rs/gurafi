//! Shared helpers.

use syn::Attribute;

/// Collect `#[doc = "..."]` lines into one description, following Rust's convention that a
/// leading space is stripped from each line.
pub fn doc_description(attrs: &[Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(name_value) = &attr.meta else {
            continue;
        };
        let syn::Expr::Lit(expr) = &name_value.value else {
            continue;
        };
        let syn::Lit::Str(literal) = &expr.lit else {
            continue;
        };
        let line = literal.value();
        let line = line.strip_prefix(' ').unwrap_or(&line);
        lines.push(line.to_owned());
    }
    if lines.is_empty() {
        return None;
    }
    let text = lines.join("\n");
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Check that a string is a legal GraphQL name and produce an actionable error if it is not.
pub fn validate_graphql_name(name: &str, span: proc_macro2::Span) -> syn::Result<()> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(first) => {
            (first == '_' || first.is_ascii_alphabetic())
                && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        }
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(syn::Error::new(
            span,
            format!(
                "`{name}` is not a valid GraphQL name; names must start with a letter or `_` and \
                 contain only letters, digits and `_`"
            ),
        ))
    }
}

/// The scalar names the specification reserves.
const BUILT_IN_SCALARS: [&str; 5] = ["String", "Int", "Float", "Boolean", "ID"];

/// Check a derived type's GraphQL name.
///
/// Built-in scalars are rejected with a hint, because shadowing one would make every field that
/// uses the real scalar ambiguous and is almost never what the author meant.
pub fn validate_type_name(name: &str, span: proc_macro2::Span) -> syn::Result<()> {
    validate_graphql_name(name, span)?;
    if BUILT_IN_SCALARS.contains(&name) {
        return Err(syn::Error::new(
            span,
            format!(
                "`{name}` is a built-in GraphQL scalar and cannot be redefined by a derive. \
                 Remove `#[graphql(name = \"{name}\")]` to use the Rust type's own name, or choose \
                 a different name such as `{name}Value`."
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_type_name;

    #[test]
    fn built_in_scalar_names_are_rejected_with_a_hint() {
        let span = proc_macro2::Span::call_site();
        for name in ["String", "Int", "Float", "Boolean", "ID"] {
            let error = validate_type_name(name, span).expect_err(name);
            let message = error.to_string();
            assert!(message.contains("built-in GraphQL scalar"), "{message}");
            assert!(message.contains("different name"), "{message}");
        }
    }

    #[test]
    fn ordinary_names_are_accepted() {
        let span = proc_macro2::Span::call_site();
        validate_type_name("Giraffe", span).expect("Giraffe");
        validate_type_name("StringValue", span).expect("StringValue");
    }
}
