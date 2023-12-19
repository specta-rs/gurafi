use std::future::{Future, Ready};

pub trait ResolverFn<M> {
    type Function: Fn(M) -> Self::Result;
    type Future: Future<Output = Self::Result>;
    type Result;
}

// TODO: Copy the really good errors from rspc's beta stuff and setup trybuild to enforce them

// TODO: Allow async or sync functions
// TODO: Allow results
// TODO: Context type. How is that gonna be inferred through?

// TODO: What constrains exist on the return type?
// - Really all we need is a way to infer GraphQL type and Serialize
//
// - What happens if the user returns a `serde_json::Value` or `HashMap`???

impl<M, F, T> ResolverFn<M> for F
where
    F: Fn(M) -> T,
{
    type Function = F;
    type Future = Ready<T>;
    type Result = T;
}
