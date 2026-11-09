use std::fmt;

pub struct SerializeSeq<'a>(pub(crate) &'a mut dyn erased_serde::SerializeSeq);

impl<'a> fmt::Debug for SerializeSeq<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerializeSeq").finish()
    }
}

// TODO: Error type
impl<'a> SerializeSeq<'a> {
    pub fn serialize_element<T: serde::Serialize>(&mut self, element: &T) -> Result<(), ()> {
        self.0.erased_serialize_element(element).map_err(|_| ())
    }

    pub fn end(self) {
        self.0.erased_end();
    }
}
