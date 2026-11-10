//! Relay mutation payloads.
//!
//! Relay mutations conventionally take one `input` object and return a payload containing
//! `result` plus `clientMutationId`. [`Relay::mutation`] generates the payload object and wires
//! the root field:
//!
//! ```text
//! createUser(input: CreateUserInput!, clientMutationId: String): CreateUserPayload!
//! CreateUserPayload { result: User!, clientMutationId: String }
//! ```
//!
//! Unlike Pothos, `clientMutationId` is a sibling field argument rather than a member of the input
//! object, because `gurafi` input object names are compile-time constants. This keeps
//! `CreateUserInput` exactly the type the application declared while still matching Relay's
//! response shape.

use std::sync::Arc;

use gurafi::{
    BoxFuture, InputObject, ObjectModel, OutputType, Schema,
    gql::{self, GraphQLError, InputValue, Object as GqlObject, TypeRef, Value},
};

use crate::{Relay, RelayError, connection::validate_name};

/// How a Relay mutation handles `clientMutationId`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientMutationId {
    /// Do not expose the field or argument.
    Omit,
    /// Expose a nullable `clientMutationId` argument and payload field.
    #[default]
    Optional,
    /// Require a non-null `clientMutationId`.
    Required,
}

/// Configuration for a generated Relay mutation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MutationConfig {
    /// How `clientMutationId` is exposed.
    pub client_mutation_id: ClientMutationId,
}

impl MutationConfig {
    /// Create a configuration with an optional `clientMutationId`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `clientMutationId` mode.
    #[must_use]
    pub fn client_mutation_id(mut self, mode: ClientMutationId) -> Self {
        self.client_mutation_id = mode;
        self
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct PayloadMarker;

impl ObjectModel for PayloadMarker {
    const NAME: &'static str = "__GurafiRelayMutationPayload";
}

impl OutputType for PayloadMarker {
    fn output_type() -> TypeRef {
        TypeRef::named(Self::NAME).non_null()
    }
}

/// The generated argument object of a Relay mutation.
///
/// The const parameter encodes [`ClientMutationId`]: `0` omit, `1` optional, `2` required. It
/// exists so `InputObject::fields` (which has no `self`) can describe the right argument list.
#[doc(hidden)]
pub struct MutationArgs<A: InputObject, const CLIENT: u8> {
    /// The application's input object.
    pub input: A,
    /// The Relay client mutation ID.
    pub client_mutation_id: Option<String>,
}

impl<A: InputObject, const CLIENT: u8> InputObject for MutationArgs<A, CLIENT> {
    const NAME: Option<&'static str> = None;

    fn fields() -> Result<Vec<InputValue>, GraphQLError> {
        let name = A::NAME.ok_or_else(|| {
            GraphQLError::new(
                "a Relay mutation input must be a named input object; add \
                 `#[graphql(name = \"...\")]` to the input type",
            )
        })?;
        let mut fields = vec![InputValue::new("input", TypeRef::named(name).non_null())];
        match CLIENT {
            0 => {}
            1 => fields.push(InputValue::new("clientMutationId", TypeRef::string())),
            2 => fields.push(InputValue::new(
                "clientMutationId",
                TypeRef::string().non_null(),
            )),
            _ => {
                return Err(GraphQLError::new(
                    "invalid internal `clientMutationId` configuration",
                ));
            }
        }
        Ok(fields)
    }

    fn from_object(input: &gql::Object) -> Result<Self, GraphQLError> {
        let Some(value) = input.get("input") else {
            return Err(GraphQLError::new("Missing required argument `input`."));
        };
        let value = match value {
            Value::Object(object) => A::from_object(object)?,
            Value::Null => return Err(GraphQLError::new("Argument `input` must not be null.")),
            other => {
                return Err(GraphQLError::new(format!(
                    "Argument `input` must be an input object, but found {}.",
                    other.kind()
                )));
            }
        };

        let client_mutation_id = match CLIENT {
            0 => None,
            1 => match input.get("clientMutationId") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| {
                            GraphQLError::new("Argument `clientMutationId` must be a string.")
                        })?
                        .to_owned(),
                ),
            },
            2 => match input.get("clientMutationId") {
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| {
                            GraphQLError::new("Argument `clientMutationId` must be a string.")
                        })?
                        .to_owned(),
                ),
                None => {
                    return Err(GraphQLError::new(
                        "Missing required argument `clientMutationId`.",
                    ));
                }
            },
            _ => {
                return Err(GraphQLError::new(
                    "invalid internal `clientMutationId` configuration",
                ));
            }
        };

        Ok(Self {
            input: value,
            client_mutation_id,
        })
    }

    fn validate(&self) -> Result<(), GraphQLError> {
        self.input.validate()
    }

    fn register_args(registry: &mut gurafi::TypeRegistry) {
        A::register_args(registry);
    }
}

fn require_named_input<A: InputObject>() -> Result<(), RelayError> {
    if A::NAME.is_none() {
        return Err(RelayError::new(
            "a Relay mutation input must be a named input object; add \
             `#[graphql(name = \"...\")]` to the input type",
        ));
    }
    Ok(())
}

fn payload_name(field: &str) -> String {
    let mut chars = field.chars();
    let Some(first) = chars.next() else {
        return "Payload".to_owned();
    };
    let mut name = String::new();
    name.push(first.to_ascii_uppercase());
    name.push_str(chars.as_str());
    name.push_str("Payload");
    name
}

impl<Ctx: 'static + Sync> Relay<Ctx> {
    /// Register a Relay mutation with an optional `clientMutationId`.
    ///
    /// `input` is the field's single input object argument, and the resolver receives that typed
    /// input plus the context. The generated payload is an object containing `result` and,
    /// depending on configuration, `clientMutationId`.
    ///
    /// # Errors
    ///
    /// Returns an error when `A` is not a named input object, or the payload name was already
    /// registered with this Relay.
    pub fn mutation<T, A, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        resolver: F,
    ) -> Result<(), RelayError>
    where
        T: OutputType + gurafi::IntoValue + Send + 'static,
        A: InputObject + Send + 'static,
        F: Fn(A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        self.mutation_with(schema, field, MutationConfig::default(), resolver)
    }

    /// Register a Relay mutation with explicit `clientMutationId` behaviour.
    pub fn mutation_with<T, A, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        config: MutationConfig,
        resolver: F,
    ) -> Result<(), RelayError>
    where
        T: OutputType + gurafi::IntoValue + Send + 'static,
        A: InputObject + Send + 'static,
        F: Fn(A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        require_named_input::<A>()?;
        validate_name(field)?;
        validate_name(&payload_name(field))?;
        let payload = self.register_payload::<T>(schema, field, config)?;
        match config.client_mutation_id {
            ClientMutationId::Omit => {
                self.install_mutation::<T, A, 0, _>(schema, &payload, field, config, resolver);
            }
            ClientMutationId::Optional => {
                self.install_mutation::<T, A, 1, _>(schema, &payload, field, config, resolver);
            }
            ClientMutationId::Required => {
                self.install_mutation::<T, A, 2, _>(schema, &payload, field, config, resolver);
            }
        }
        Ok(())
    }

    /// Async version of [`Relay::mutation`].
    pub fn mutation_async<T, A, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        resolver: F,
    ) -> Result<(), RelayError>
    where
        T: OutputType + gurafi::IntoValue + Send + 'static,
        A: InputObject + Send + 'static,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>> + Send + Sync + 'static,
    {
        self.mutation_async_with(schema, field, MutationConfig::default(), resolver)
    }

    /// Async version of [`Relay::mutation_with`].
    pub fn mutation_async_with<T, A, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        config: MutationConfig,
        resolver: F,
    ) -> Result<(), RelayError>
    where
        T: OutputType + gurafi::IntoValue + Send + 'static,
        A: InputObject + Send + 'static,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>> + Send + Sync + 'static,
    {
        require_named_input::<A>()?;
        validate_name(field)?;
        validate_name(&payload_name(field))?;
        let payload = self.register_payload::<T>(schema, field, config)?;
        match config.client_mutation_id {
            ClientMutationId::Omit => {
                self.install_mutation_async::<T, A, 0, _>(
                    schema, &payload, field, config, resolver,
                );
            }
            ClientMutationId::Optional => {
                self.install_mutation_async::<T, A, 1, _>(
                    schema, &payload, field, config, resolver,
                );
            }
            ClientMutationId::Required => {
                self.install_mutation_async::<T, A, 2, _>(
                    schema, &payload, field, config, resolver,
                );
            }
        }
        Ok(())
    }

    fn register_payload<T: OutputType + Send + 'static>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        config: MutationConfig,
    ) -> Result<TypeRef, RelayError> {
        let name = payload_name(field);
        if self.payload_names.iter().any(|existing| existing == &name) {
            return Err(RelayError::new(format!(
                "mutation payload `{name}` is already registered with this Relay"
            )));
        }
        let payload = schema.object_ref::<PayloadMarker>(name.clone());
        schema.implement(&payload, |object| {
            // A nullable Rust result (`Option<T>`) produces a nullable `result`, matching the
            // resolver's contract; everything else is non-null.
            if T::is_optional() {
                object
                    .raw_field::<gurafi::Raw<T>>("result")
                    .as_dynamic(T::output_type());
            } else {
                object
                    .raw_field::<gurafi::Raw<T>>("result")
                    .non_null()
                    .as_dynamic(T::output_type().non_null());
            }
            match config.client_mutation_id {
                ClientMutationId::Omit => {}
                ClientMutationId::Optional => {
                    object.raw_field::<Option<String>>("clientMutationId");
                }
                ClientMutationId::Required => {
                    object.raw_field::<String>("clientMutationId");
                }
            }
        });
        self.payload_names.push(name);
        Ok(payload.type_ref().non_null())
    }

    fn install_mutation<T, A, const CLIENT: u8, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        payload: &TypeRef,
        field: &'static str,
        config: MutationConfig,
        resolver: F,
    ) where
        T: OutputType + gurafi::IntoValue + Send + 'static,
        A: InputObject + Send + 'static,
        F: Fn(A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        let payload_type = payload.clone();
        schema.mutation(|mutation| {
            let mut field = mutation
                .field_result::<gurafi::Raw<PayloadMarker>, MutationArgs<A, CLIENT>, _>(
                    field,
                    move |args: MutationArgs<A, CLIENT>, ctx: &Ctx| {
                        let result = resolver(args.input, ctx)?;
                        payload_value(result, args.client_mutation_id, config)
                    },
                );
            field.as_dynamic(payload_type.clone());
        });
    }

    fn install_mutation_async<T, A, const CLIENT: u8, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        payload: &TypeRef,
        field: &'static str,
        config: MutationConfig,
        resolver: F,
    ) where
        T: OutputType + gurafi::IntoValue + Send + 'static,
        A: InputObject + Send + 'static,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>> + Send + Sync + 'static,
    {
        let resolver = Arc::new(resolver);
        schema.mutation(|mutation| {
            let mut field_config = mutation
                .field_result_async::<gurafi::Raw<PayloadMarker>, MutationArgs<A, CLIENT>, _>(
                    field,
                    move |args: MutationArgs<A, CLIENT>, ctx: &Ctx| {
                        let resolver = Arc::clone(&resolver);
                        Box::pin(async move {
                            let result = resolver(args.input, ctx).await?;
                            payload_value(result, args.client_mutation_id, config)
                        })
                    },
                );
            field_config.as_dynamic(payload.clone());
        });
    }
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "relay.mutation",
        fields(
            result_type = std::any::type_name::<T>(),
            has_client_mutation_id = client_mutation_id.is_some(),
        )
    )
)]
fn payload_value<T: gurafi::IntoValue>(
    result: T,
    client_mutation_id: Option<String>,
    config: MutationConfig,
) -> Result<gurafi::Raw<PayloadMarker>, GraphQLError> {
    let mut object = GqlObject::new();
    object.insert("result", result.into_value()?);
    if config.client_mutation_id != ClientMutationId::Omit {
        object.insert(
            "clientMutationId",
            match client_mutation_id {
                Some(value) => Value::String(Arc::from(value)),
                None => Value::Null,
            },
        );
    }
    Ok(gurafi::Raw::from(Value::Object(Arc::new(object))))
}
