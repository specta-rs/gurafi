use crate::ModelBuilder;

pub trait Model: Sized {
    fn define(t: ModelBuilder<Self>) -> ModelBuilder<Self>;
}
