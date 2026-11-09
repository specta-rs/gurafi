// TODO: Possibly break out into a dedicated crate

use std::io;

use crate::{SerializeMap, SerializeMap2, Serializer, Serializer2};

pub struct Json<W> {
    writer: W,
    map_len_stack: Vec<u8>, // TODO: Stack allocated for first x items
}

impl<W> Json<W>
where
    W: io::Write,
{
    pub fn new(writer: W) -> Self {
        Json {
            writer,
            map_len_stack: Vec::new(),
        }
    }
}

impl<W> Serializer for Json<W>
where
    W: io::Write,
{
    fn serialize_str(&mut self, value: &str) -> Result<(), ()> {
        write!(self.writer, "\"{}\"", value).map_err(|_| ())
    }

    fn serialize_map(&mut self) -> Result<SerializeMap2, ()> {
        self.map_len_stack.push(0);
        write!(self.writer, "{{").map_err(|_| ())?;
        Ok(SerializeMap2::new(self))
    }
}

impl<W> SerializeMap for Json<W>
where
    W: io::Write,
{
    fn serialize_entry(&mut self, key: String) -> Result<Serializer2, ()> {
        let len = self.map_len_stack.last_mut().unwrap();
        if *len != 0 {
            write!(self.writer, ", ").map_err(|_| ())?;
        }
        *len += 1;
        write!(self.writer, "\"{}\": ", key).map_err(|_| ())?;
        Ok(Serializer2::new(self))
    }

    fn end(&mut self) {
        let _ = self.map_len_stack.pop(); // TODO: Panic in debug????

        write!(self.writer, "}}").unwrap();
    }
}
