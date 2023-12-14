use std::{borrow::Cow, collections::HashMap};

use crate::{InputType, OutputType, Thunk};

/// https://graphql.org/graphql-js/type/#graphqlobjecttype
#[derive(Debug)]
pub struct ObjectType {
    pub name: Cow<'static, str>,
    pub description: Option<Cow<'static, str>>,
    // interfaces?: GraphQLInterfacesThunk | Array<GraphQLInterfaceType>;
    // isTypeOf?: (value: any, info?: GraphQLResolveInfo) => boolean;
    pub fields: Thunk<HashMap<Cow<'static, str>, FieldConfig>>,
}

#[derive(Debug)]
pub struct FieldConfig {
    pub description: Option<Cow<'static, str>>,
    pub deprecation_reason: Option<Cow<'static, str>>,
    pub args: Option<HashMap<Cow<'static, str>, ArgumentConfig>>,
    pub ty: OutputType,
    // TODO:
    // type GraphQLFieldResolveFn = (
    //     source?: any,
    //     args?: {[argName: string]: any},
    //     context?: any,
    //     info?: GraphQLResolveInfo
    //   ) => any
    pub resolve: Option<fn() -> String>,
}

#[derive(Debug)]
pub struct ArgumentConfig {
    pub description: Option<Cow<'static, str>>,
    pub ty: InputType,
    // defaultValue?: any;
}
