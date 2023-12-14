use std::{borrow::Cow, collections::HashMap};

use crate::{InputType, OutputType, Thunk};

/// https://graphql.org/graphql-js/type/#graphqlobjecttype
pub struct ObjectType {
    name: Cow<'static, str>,
    description: Option<Cow<'static, str>>,
    // interfaces?: GraphQLInterfacesThunk | Array<GraphQLInterfaceType>;
    // isTypeOf?: (value: any, info?: GraphQLResolveInfo) => boolean;
    fields: Thunk<HashMap<Cow<'static, str>, FieldConfig>>,
}

pub struct FieldConfig {
    description: Option<Cow<'static, str>>,
    deprecation_reason: Option<Cow<'static, str>>,
    args: Option<HashMap<Cow<'static, str>, ArgumentConfig>>,
    ty: OutputType,
    // TODO:
    // type GraphQLFieldResolveFn = (
    //     source?: any,
    //     args?: {[argName: string]: any},
    //     context?: any,
    //     info?: GraphQLResolveInfo
    //   ) => any
    resolve: Option<fn() -> String>,
}

pub struct ArgumentConfig {
    description: Option<Cow<'static, str>>,
    ty: InputType,
    // defaultValue?: any;
}
