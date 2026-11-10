# Additive `gurafi` API used by this crate

`gurafi-relay` is written against the public `gurafi` builder. One tiny read-only addition was
needed to implement the "add an `id` field only when the object does not already declare one"
behaviour and to avoid duplicate fields when the same interface is reopened:

- `gurafi::ObjectBuilder::has_field(&self, name: &str) -> bool`
- `gurafi::InterfaceBuilder::has_field(&self, name: &str) -> bool`

Both inspect the definition currently held by the builder and never mutate it. They do not change
registration, validation or execution. Everything else in this crate uses existing public APIs
(`Schema::object_ref`, `Schema::implement`, `ObjectBuilder::field`, `FieldConfig::as_dynamic`, the
`ObjectModel`/`OutputType`/`IntoValue`/`InputObject` traits, and `RequestCache`).

No changes to `gql` or `suku` are required.
