#[macro_export]
macro_rules! expose {
    ($ty_ident:ty : $field_ident:ident) => {{
        #[allow(unused)]
        fn _lsp_hint(_model_ty: $ty_ident) {
            let _ = _model_ty.$field_ident;
        }

        todo!()
    }};
}

#[cfg(test)]
mod tests {
    pub struct Foo {
        bar: String,
    }

    pub struct Bar {
        foo: String,
    }

    #[test]
    #[ignore = "`expose!` is not implemented yet"]
    fn test_expose() {
        // Test same-module
        expose!(Foo:bar);

        // Test cross-module path
        expose!(self::Foo:bar);
    }

    #[test]
    fn test_expose_ties_to_model() {
        // TODO: Check `expose` can only refer to fields on the correct backing model.

        // TODO: Do this without introducing model generic on `FieldBuilder`
    }
}
