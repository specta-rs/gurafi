use gurafi::Schema;

// We assume this is an external crate, hence we can't do impl's on it.
mod external {
    pub struct CustomString(String);
}

fn main() {
    let mut schema = <Schema>::new();

    todo!();
}
