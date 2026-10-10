pub trait Store {
    fn put(&mut self, doc: &str);
    fn get(&self, id: &str) -> Option<String>;
    fn all(&self) -> Vec<String>;
}

#[derive(Default)]
pub struct MemStore {
    docs: Vec<String>,
}

impl Store for MemStore {
    fn put(&mut self, doc: &str) {
        self.docs.push(doc.to_string());
    }

    fn get(&self, id: &str) -> Option<String> {
        self.docs.iter().find(|d| d.contains(id)).cloned()
    }

    fn all(&self) -> Vec<String> {
        self.docs.clone()
    }
}

/// A second trait with an `all` method: `MemStore::all` exists twice, once per trait impl.
pub trait Snapshot {
    fn all(&self) -> usize;
}

impl Snapshot for MemStore {
    fn all(&self) -> usize {
        self.docs.len()
    }
}

pub struct Page<T>(pub Vec<T>);

impl<T> Page<T> {
    pub fn new() -> Self {
        Page(Vec::new())
    }
}
