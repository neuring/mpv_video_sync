use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;

use async_std::task;
use structopt::StructOpt;

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

#[derive(Debug, StructOpt)]
pub struct Config {
    /// URL of synchronizing server.
    network_url: String,

    /// Path to video which to play.
    video_path: PathBuf,

    #[structopt(long, short = "s", default_value = "/tmp/mpv_sync_socket")]
    /// IPC socket location for communication with mpv.
    ipc_socket: PathBuf,

    #[structopt(long, short = "m", default_value = "/usr/bin/mpv")]
    /// mpv command to execute. This can be used to specify a different mpv binary
    /// or add flags.
    mpv_command: MpvProcessInvocation,
}

fn main() {
    let config = Config::from_args();
    dbg!(&config);

    let mut command = Command::new(&config.mpv_command.command);

    let mut mpv_ipc_flag = OsString::new();
    mpv_ipc_flag.push("--input-ipc-server=");
    mpv_ipc_flag.push(config.ipc_socket.as_os_str());

    command
        .args(&config.mpv_command.flags)
        .arg("--keep-open=always")
        .arg(mpv_ipc_flag)
        .arg(&config.video_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut handle = command.spawn().expect("Couldn't start mpv");

    std::thread::spawn(|| {
        task::block_on(async {
            backend::start_backend(Arc::new(config)).await.unwrap()
        })
    });

    let status = handle.wait().unwrap();
    //TODO: implement clean shutdown, by sending a signal to all running threads, somehow...
    std::process::exit(status.code().unwrap_or(0));
}
