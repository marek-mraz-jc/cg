mod bus;
mod store;

use bus::Bus;
use store::{MemStore, Store};

type Handler = fn(&str) -> String;

#[derive(Default)]
pub struct Router {
    routes: Vec<(String, &'static str, Handler)>,
}

pub struct MethodRouter(Vec<(&'static str, Handler)>);

pub fn get(h: Handler) -> MethodRouter {
    MethodRouter(vec![("GET", h)])
}

pub fn post(h: Handler) -> MethodRouter {
    MethodRouter(vec![("POST", h)])
}

impl MethodRouter {
    pub fn post(mut self, h: Handler) -> Self {
        self.0.push(("POST", h));
        self
    }

    pub fn delete(mut self, h: Handler) -> Self {
        self.0.push(("DELETE", h));
        self
    }
}

impl Router {
    pub fn route(mut self, path: &str, m: MethodRouter) -> Self {
        for (verb, h) in m.0 {
            self.routes.push((path.to_string(), verb, h));
        }
        self
    }

    pub fn nest(self, _prefix: &str, inner: Router) -> Self {
        Router { routes: [self.routes, inner.routes].concat() }
    }
}

fn list_entities(_: &str) -> String {
    MemStore::default().all().join(",")
}

fn create_entity(body: &str) -> String {
    let mut s = MemStore::default();
    s.put(body);
    Bus.publish("entities.created", body);
    "201".into()
}

fn get_entity(id: &str) -> String {
    MemStore::default().get(id).unwrap_or_default()
}

fn delete_entity(id: &str) -> String {
    Bus.publish("entities.deleted", id);
    "204".into()
}

fn on_stats(payload: &str) {
    println!("stats {payload}");
}

pub fn app() -> Router {
    let api = Router::default()
        .route("/entities", get(list_entities).post(create_entity))
        .route("/entities/{id}", get(get_entity).delete(delete_entity));
    Bus.subscribe("stats.updated", on_stats);
    Router::default().nest("/api", api)
}

fn main() {
    println!("{}", app().routes.len());
}

#[cfg(test)]
mod tests {
    #[test]
    fn creates() {
        assert_eq!(super::create_entity("x"), "201");
    }
}
