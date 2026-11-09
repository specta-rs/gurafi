use core::fmt;
use std::{any::Any, borrow::Cow, collections::HashMap, future::Future, pin::Pin};

use crate::{interface_type::InterfaceType, serializer::Serializer, InputType, OutputType, Thunk};

/// https://graphql.org/graphql-js/type/#graphqlobjecttype
#[derive(Debug)]
pub struct ObjectType {
    pub name: Cow<'static, str>,
    pub description: Option<Cow<'static, str>>,
    pub interfaces: Option<Thunk<Vec<InterfaceType>>>,
    pub is_type_of: Option<fn(value: &dyn Any, info: GraphQLResolveInfo) -> bool>,
    pub fields: Thunk<HashMap<Cow<'static, str>, FieldConfig>>,
}

// impl ObjectType {
//     pub fn new(name: Cow<'static, str>) -> Self {
//         Self {
//             name,
//             description: None,
//             interfaces: None,
//             is_type_of: None,
//             fields: Thunk::new(HashMap::new),
//         }
//     }
// }

#[derive(Debug)]
pub struct GraphQLResolveInfo<'a> {
    pub field_name: &'a str,
    // TODO: Finish this
    //   fieldNodes: Array<Field>,
    //   returnType: GraphQLOutputType,
    //   parentType: GraphQLCompositeType,
    //   schema: GraphQLSchema,
    //   fragments: { [fragmentName: string]: FragmentDefinition },
    //   rootValue: any,
    //   operation: OperationDefinition,
    //   variableValues: { [variableName: string]: any },
}

#[derive(Debug)]
pub struct ResolveFnArgs<'a> {
    // TODO: Maybe a custom deserialize type
    // pub args: &'a HashMap<Cow<'static, str>, serde_json::Value>,
    pub context: &'a dyn Any,
    pub info: GraphQLResolveInfo<'a>,
}

// TODO: Handle the case of forgetting to call `OwnedSerializer`
// TODO: Should we relax `Sync` bounds and rely on something like: https://docs.rs/sync_wrapper/latest/sync_wrapper/
pub enum ResolveFn {
    Ptr(fn(ResolveFnArgs, Serializer)),
    Boxed(Box<dyn Fn(ResolveFnArgs, Serializer) + Send + Sync>),
    PtrAsync(fn(ResolveFnArgs, Serializer) -> Pin<Box<dyn Future<Output = ()>>>),
    BoxedAsync(
        Box<dyn Fn(ResolveFnArgs, Serializer) -> Pin<Box<dyn Future<Output = ()>>> + Send + Sync>,
    ),
}

// TODO: `From` impls for `FieldConfigResolveFn`

impl fmt::Debug for ResolveFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveFn::Ptr(v) => f.debug_tuple("FieldConfigResolveFn::Ptr").field(v).finish(),
            ResolveFn::Boxed(_) => f.debug_tuple("FieldConfigResolveFn::Boxed").finish(),
            ResolveFn::PtrAsync(v) => f
                .debug_tuple("FieldConfigResolveFn::AsyncPtr")
                .field(v)
                .finish(),
            ResolveFn::BoxedAsync(_) => f.debug_tuple("FieldConfigResolveFn::AsyncBoxed").finish(),
        }
    }
}

impl ResolveFn {
    pub async fn exec(&self, args: ResolveFnArgs<'_>, result: Serializer<'_>) {
        match self {
            ResolveFn::Ptr(v) => v(args, result),
            ResolveFn::Boxed(v) => v(args, result),
            ResolveFn::PtrAsync(v) => v(args, result).await,
            ResolveFn::BoxedAsync(v) => v(args, result).await,
        }
    }
}

// TODO: What if `ty` and `resolve` don't match??? Should we enforce it???
#[derive(Debug)]
pub struct FieldConfig {
    pub description: Option<Cow<'static, str>>,
    pub deprecation_reason: Option<Cow<'static, str>>,
    pub args: Option<HashMap<Cow<'static, str>, ArgumentConfig>>,
    pub ty: OutputType,
    pub resolve: Option<ResolveFn>,
}

#[derive(Debug)]
pub struct ArgumentConfig {
    pub description: Option<Cow<'static, str>>,
    pub ty: InputType,
    pub default_value: Option<fn(Serializer)>,
}
