/// > Thunks are primarily used to delay a calculation until its result is needed
/// https://en.wikipedia.org/wiki/Thunk
pub enum Thunk<T> {
    Lazy(Box<dyn Fn() -> T>),
    Eager(T),
}
