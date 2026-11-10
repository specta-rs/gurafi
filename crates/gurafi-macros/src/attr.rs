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
    /// `#[graphql(one_of)]`, only meaningful on input objects.
    pub one_of: bool,
    /// `#[graphql(validate = "path")]`, only meaningful on input objects.
    pub validate: Option<Path>,
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
    /// `#[graphql(validate = "path")]`, only meaningful on input fields.
    pub validate: Option<Path>,
}

/// Reject attributes that only make sense on an input object.
pub fn reject_input_only(attrs: &TypeAttrs, spanned: impl quote::ToTokens) -> syn::Result<()> {
    if attrs.one_of {
        return Err(syn::Error::new_spanned(
            spanned,
            "`one_of` only applies to `#[derive(Input)]`",
        ));
    }
    if attrs.validate.is_some() {
        return Err(syn::Error::new_spanned(
            spanned,
            "`validate` only applies to `#[derive(Input)]`",
        ));
    }
    Ok(())
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
            } else if meta.path.is_ident("one_of") {
                parsed.one_of = true;
                Ok(())
            } else if meta.path.is_ident("validate") {
                parsed.validate = Some(parse_path(&meta)?);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[graphql]` attribute here; expected `name`, `description`, \
                     `specified_by`, `one_of` or `validate`",
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
            } else if meta.path.is_ident("validate") {
                parsed.validate = Some(parse_path(&meta)?);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[graphql]` attribute here; expected `name`, `description`, \
                     `default`, `default_with`, `deprecated` or `validate`",
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

/// Parse `= "path::to::function"` into a path.
fn parse_path(meta: &ParseNestedMeta<'_>) -> syn::Result<Path> {
    let lit = parse_lit_str(meta)?;
    lit.parse::<Path>()
        .map_err(|error| syn::Error::new(lit.span(), format!("expected a function path: {error}")))
}
