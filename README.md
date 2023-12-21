# MPV video synchronization

This project contains a server and client for syncing MPV instances on different computers.

## Building

A somewhat recent rust toolchain (>= 1.65) is required to build this project.

```shell
cargo build --release
```

This command builds both the client and server. The respective binaries are located inside `target/release` as `mpv_sync_client` and `mpv_sync_server`.

## Running

On your server run the command 
```shell
./mpv_sync_server <server-addr>:<port>
```

Each client runs:
```shell
./mpv_sync_client <server-addr>:<port> <video-file>
```

It is assumed that each client has `mpv` installed.
Furthermore, by default, the os username is chosen as the username.
This behavior can be changed using the `-u` flag.
