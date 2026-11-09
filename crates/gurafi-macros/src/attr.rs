//! Parsing of the `#[graphql(...)]` attribute.

use syn::{Attribute, Expr, LitStr, Path, Token, meta::ParseNestedMeta};

use crate::util::doc_description;

/// Attributes accepted on a type (object, input, enum, scalar, union).
#[derive(Default)]
pub struct TypeAttrs {
    /// `#[graphql(name = "...")]`
    pub name: Option<LitStr>,
    /// `#[graphql(description = "...")]`
    pub description: Option<LitStr>,
    /// `#[graphql(specified_by = "...")]`, only meaningful on scalars.
    pub specified_by: Option<LitStr>,
}

/// Attributes accepted on an input field or enum variant.
#[derive(Default)]
pub struct FieldAttrs {
    /// `#[graphql(name = "...")]`
    pub name: Option<LitStr>,
    /// `#[graphql(description = "...")]`
    pub description: Option<LitStr>,
    /// `#[graphql(default = expr)]`
    pub default: Option<Expr>,
    /// `#[graphql(default_with = "path")]`
    pub default_with: Option<Path>,
    /// Deprecation reason. `None` means not deprecated, `Some(None)` means deprecated with
    /// the default reason.
    pub deprecated: Option<Option<LitStr>>,
}

/// Parse the type-level attributes of a derive input.
pub fn parse_type_attrs(attrs: &[Attribute]) -> syn::Result<TypeAttrs> {
    let mut parsed = TypeAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("graphql") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                parsed.name = Some(parse_lit_str(&meta)?);
                Ok(())
            } else if meta.path.is_ident("description") {
                parsed.description = Some(parse_lit_str(&meta)?);
                Ok(())
            } else if meta.path.is_ident("specified_by") {
                parsed.specified_by = Some(parse_lit_str(&meta)?);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[graphql]` attribute here; expected `name`, `description` \
                     or `specified_by`",
                ))
            }
        })?;
    }
    if parsed.description.is_none() {
        parsed.description =
            doc_description(attrs).map(|text| LitStr::new(&text, proc_macro2::Span::call_site()));
    }
    Ok(parsed)
}

/// Parse the attributes of an input field or enum variant.
pub fn parse_field_attrs(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut parsed = FieldAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("graphql") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                parsed.name = Some(parse_lit_str(&meta)?);
                Ok(())
            } else if meta.path.is_ident("description") {
                parsed.description = Some(parse_lit_str(&meta)?);
                Ok(())
            } else if meta.path.is_ident("default") {
                let value = meta.value()?;
                parsed.default = Some(value.parse()?);
                Ok(())
            } else if meta.path.is_ident("default_with") {
                let lit = parse_lit_str(&meta)?;
                let path = lit.parse::<Path>().map_err(|error| {
                    syn::Error::new(
                        lit.span(),
                        format!("`default_with` expects a function path: {error}"),
                    )
                })?;
                parsed.default_with = Some(path);
                Ok(())
            } else if meta.path.is_ident("deprecated") {
                if meta.input.peek(Token![=]) {
                    parsed.deprecated = Some(Some(parse_lit_str(&meta)?));
                } else {
                    parsed.deprecated = Some(None);
                }
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[graphql]` attribute here; expected `name`, `description`, \
                     `default`, `default_with` or `deprecated`",
                ))
            }
        })?;
    }
    if parsed.description.is_none() {
        parsed.description =
            doc_description(attrs).map(|text| LitStr::new(&text, proc_macro2::Span::call_site()));
    }
    Ok(parsed)
}

/// The deprecation reason for a field or variant, when it is deprecated at all.
///
/// `#[graphql(deprecated)]` without a reason uses the same default as the specification's
/// `@deprecated` directive.
pub fn deprecation_reason(deprecated: &Option<Option<LitStr>>) -> Option<LitStr> {
    match deprecated {
        Some(Some(reason)) => Some(reason.clone()),
        Some(None) => Some(LitStr::new(
            "No longer supported",
            proc_macro2::Span::call_site(),
        )),
        None => None,
    }
}

fn parse_lit_str(meta: &ParseNestedMeta<'_>) -> syn::Result<LitStr> {
    meta.value()?.parse()
}
