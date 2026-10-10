pub struct Bus;

impl Bus {
    pub fn publish(&self, subject: &str, payload: &str) {
        println!("{subject} {payload}");
    }

    pub fn subscribe(&self, subject: &str, cb: fn(&str)) {
        cb(subject);
    }
}
