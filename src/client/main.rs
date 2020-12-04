use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;
use std::{convert::Infallible, ffi::OsString, fmt::Debug, fmt::Display};

use anyhow::anyhow;
use anyhow::Result;
use async_process::Command;
use async_std::{path::Path, task};
use futures::select;
use futures::FutureExt;
use rand::Rng;
use structopt::StructOpt;
use tracing::debug;
use tracing_subscriber::EnvFilter;

mod backend;
mod mpv;

#[derive(Debug)]
struct MpvProcessInvocation {
    command: String,
    flags: Vec<String>,
}

impl FromStr for MpvProcessInvocation {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut iter = s.split_whitespace().map(|e| e.to_string());

        let command = iter.next().ok_or("Command cannot be empty.".to_string())?;

        let flags = iter.collect();

        Ok(MpvProcessInvocation { command, flags })
    }
}

struct MpvSocket(String);

impl Debug for MpvSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl Display for MpvSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl AsRef<Path> for MpvSocket {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl Default for MpvSocket {
    fn default() -> Self {
        let mut rng = rand::thread_rng();
        let postfix = rng.gen_range(0, 100000);
        let res = format!("/tmp/mpv_sync_socket.{:05}", postfix);
        Self(res)
    }
}

impl FromStr for MpvSocket {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

#[derive(Debug, StructOpt)]
pub struct Config {
    /// URL of synchronizing server.
    network_url: String,

    /// Path to video which to play.
    video_path: PathBuf,

    #[structopt(long, short = "s", default_value)]
    /// IPC socket location for communication with mpv.
    ipc_socket: MpvSocket,

    #[structopt(long, short = "m", default_value = "/usr/bin/mpv")]
    /// mpv command to execute. This can be used to specify a different mpv binary
    /// or add flags.
    mpv_command: MpvProcessInvocation,
}

fn main() -> Result<()> {
    let collector = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();
    tracing::subscriber::set_global_default(collector).unwrap();

    let config = Config::from_args();

    let mut command = Command::new(&config.mpv_command.command);

    let mut mpv_ipc_flag = OsString::new();
    mpv_ipc_flag.push("--input-ipc-server=");
    mpv_ipc_flag.push(&config.ipc_socket.0);
    //dbg!(&mpv_ipc_flag);

    debug!("{}", mpv_ipc_flag.clone().into_string().unwrap());

    command
        .args(&config.mpv_command.flags)
        .arg("--keep-open=always")
        .arg(&mpv_ipc_flag)
        .arg(&config.video_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut handle = command.spawn()?;

    let backend_fut = backend::start_backend(Arc::new(config));
    let process_fut = handle.status();

    let f = async {
        select! {
            e = process_fut.fuse() => {
                debug!("MPV process has finished.");
                let e: Result<Result<(), _>, _> = e.map(|s| {
                    if !s.success() {
                        Err(anyhow!("mpv exited with error: {:?}", s.code()))
                    } else {
                        Ok(())
                    }
                }).map_err(Into::<anyhow::Error>::into);

                match e {
                    Ok(e) => e,
                    Err(e) => Err(e),
                }
            }
            e = backend_fut.fuse() => {
                debug!("Backend finished");
                handle.kill()?;
                let e: Result<()> = e.into();
                e
            }
        }
    };

    task::block_on(f)
}
