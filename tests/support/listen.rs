//! The Windows stand-in for the forward of a fake `ssh -N`. Windows' ssh stays in the
//! foreground, and Portico takes a local (`-L`) or dynamic (`-D`) forward as up once its listen
//! address is bound, so a fake given ssh's arguments `exec`s this program: it binds that
//! address as ssh would, failing as ssh does when it is taken, and holds it for a minute. With
//! no such forward (`-R`) it only waits. Tests compile it with `rustc` on first use; it is never
//! part of Portico.

use std::{env, net::TcpListener, process::exit, thread, time::Duration};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let _listener = args
        .windows(2)
        .find(|pair| pair[0] == "-L" || pair[0] == "-D")
        .map(|pair| {
            let address = listen_address(&pair[0], &pair[1]);
            TcpListener::bind(&address).unwrap_or_else(|error| {
                eprintln!("bind [{address}]: {error}");
                exit(255)
            })
        });
    thread::sleep(Duration::from_secs(60));
}

/// The address a forward listens on: `[bind:]port` for `-D`, `[bind:]port:host:hostport` for
/// `-L`, where the bind address defaults to `localhost`, as in ssh.
fn listen_address(flag: &str, spec: &str) -> String {
    let parts: Vec<&str> = spec.split(':').collect();
    let local = if flag == "-D" {
        &parts[..]
    } else {
        &parts[..parts.len().saturating_sub(2)]
    };
    match local {
        [port] => format!("127.0.0.1:{port}"),
        ["" | "localhost", port] => format!("127.0.0.1:{port}"),
        ["*", port] => format!("0.0.0.0:{port}"),
        [bind, port] => format!("{bind}:{port}"),
        _ => {
            eprintln!("unexpected forward {spec}");
            exit(255)
        }
    }
}
