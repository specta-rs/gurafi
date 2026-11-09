pub trait BackingModel: 'static {}

// TODO: What if the user does `Option<T>`, `Arc<T>`, etc? That would probs break stuff.

// TODO: Should this be implied or not???
impl<T: 'static> BackingModel for T {}
