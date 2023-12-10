pub trait ResolverFn<M> {
    type T;
}

impl<M, F, T> ResolverFn<M> for F
where
    F: Fn(M) -> T,
{
    type T = T;
}
