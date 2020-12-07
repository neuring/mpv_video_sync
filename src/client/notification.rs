use std::sync::Arc;

use super::Config;

pub struct Notificator {
    _config: Arc<Config>,
}

impl Notificator {
    pub fn new(config: Arc<Config>) -> Self {
        Self { _config: config }
    }

    pub fn notify(&self, msg: impl AsRef<str>) {
        println!("{}", msg.as_ref());
    }
}
