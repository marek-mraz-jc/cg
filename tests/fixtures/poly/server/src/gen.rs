//! Handlers a macro generates: rust-analyzer emits no references inside their bodies.
use crate::store::{MemStore, Store};

fn count(kind: &str) -> String {
    format!("{kind}:{}", MemStore::default().all().len())
}

fn wipe(kind: &str) -> String {
    MemStore::default().put(kind);
    "204".into()
}

macro_rules! kind_handlers {
    ($count:ident, $wipe:ident, $kind:literal) => {
        pub fn $count(_: &str) -> String {
            count($kind)
        }
        pub fn $wipe(_: &str) -> String {
            wipe($kind)
        }
    };
}

kind_handlers!(count_orders, wipe_orders, "order");
