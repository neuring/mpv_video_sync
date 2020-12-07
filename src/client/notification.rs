use std::sync::Arc;

use async_process::{Command, Stdio};
use tracing::warn;

use super::Config;

const NOTIFY_SEND_PATH: &'static str = "/usr/bin/notify-send";

pub struct Notificator {
    config: Arc<Config>,
    desktop_notifications_possible: bool,
}

impl Notificator {
    pub fn new(config: Arc<Config>) -> Self {
        let path = std::path::PathBuf::from(NOTIFY_SEND_PATH);
        let desktop_notifications_possible = path.exists();

        Self { 
            config ,
            desktop_notifications_possible,
        }
    }

    pub fn notify(&self, msg: impl AsRef<str>) {
        let msg = msg.as_ref();

        println!("{}", msg);

        if self.desktop_notifications_possible && !self.config.disable_desktop_notify {
            let res = Command::new(NOTIFY_SEND_PATH)
                    .arg(msg)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn();
            if let Err(e) = res {
                warn!("notify-send invocation failed: {:?}", e)
            }
        }
    }
}
