use std::{
    convert::Infallible,
    ffi::OsString,
    fmt::{Debug, Display},
    fs::File,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use async_process::Command;
use async_std::task;
use derive_more::{Display, From, FromStr};
use futures::{
    future::{select, Either},
    FutureExt,
};
use hex::ToHex;
use memmap::MmapOptions;
use rand::Rng;
use sha2::{Digest, Sha256};
use structopt::StructOpt;
use tracing::debug;
use tracing_subscriber::EnvFilter;

mod backend;
mod mpv;
mod notification;

#[derive(Debug)]
struct MpvProcessInvocation {
    command: String,
    flags: Vec<String>,
}

impl FromStr for MpvProcessInvocation {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut iter = s.split_whitespace().map(|e| e.to_string());

        let command = iter
            .next()
            .ok_or_else(|| "Command cannot be empty.".to_string())?;

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

impl AsRef<async_std::path::Path> for MpvSocket {
    fn as_ref(&self) -> &async_std::path::Path {
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

#[derive(Debug, From, Display, PartialEq, Eq, FromStr)]
struct Username(String);

impl Default for Username {
    fn default() -> Self {
        std::env::var("USER")
            .ok()
            .to_owned()
            .unwrap_or_else(|| {
                let mut rng = rand::thread_rng();
                let postfix = rng.gen_range(0, 100000);
                format!("Anonymous-{:05}", postfix)
            })
            .into()
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

    /// Name used to represent your client.
    #[structopt(long, short = "u", default_value)]
    username: Username,

    /// Use desktop notifications when changes occur.
    #[structopt(long, short = "n")]
    disable_desktop_notify: bool,

    #[structopt(skip)]
    video_hash: String,
}

fn calculate_file_hash(path: impl AsRef<Path>) -> Result<String> {
    let file = File::open(&path).with_context(|| {
        format!("Failed to open file {}", path.as_ref().to_string_lossy())
    })?;

    let mmap = unsafe { MmapOptions::new().map(&file)? };

    let mut hasher = Sha256::new();

    hasher.update(&mmap[..]);

    let hash = hasher.finalize();

    let hash = hash.as_slice().encode_hex::<String>();

    Ok(hash)
}

fn main() -> Result<()> {
    let collector = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();
    tracing::subscriber::set_global_default(collector).unwrap();

    // Construct Config
    let mut config = Config::from_args();

    let hash = calculate_file_hash(&config.video_path)
        .context("Couldn't calculate hash of video.")?;

    config.video_hash = hash;

    // Start MPV
    let mut command = Command::new(&config.mpv_command.command);

    let mut mpv_ipc_flag = OsString::new();
    mpv_ipc_flag.push("--input-ipc-server=");
    mpv_ipc_flag.push(&config.ipc_socket.0);

    debug!("{}", mpv_ipc_flag.clone().into_string().unwrap());

    command
        .args(&config.mpv_command.flags)
        .arg("--keep-open=always")
        .arg(&mpv_ipc_flag)
        .arg(&config.video_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut handle = command.spawn()?;

    let backend_fut = backend::start_backend(Arc::new(config)).boxed();
    let process_fut = handle.status().boxed();

    let f = async {
        match select(process_fut, backend_fut).await {
            Either::Left((e, _)) => {
                debug!("MPV process has finished.");
                let e: Result<Result<(), _>, _> = e
                    .map(|s| {
                        if !s.success() {
                            Err(anyhow!("mpv exited with error: {:?}", s.code()))
                        } else {
                            Ok(())
                        }
                    })
                    .map_err(Into::<anyhow::Error>::into);

                match e {
                    Ok(e) => e,
                    Err(e) => Err(e),
                }
            }
            Either::Right((e, _)) => {
                debug!("Backend finished");
                handle.kill()?;
                let e: Result<()> = e;
                e
            }
        }
    };

    task::block_on(f)
}
