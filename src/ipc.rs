use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use traccia::warn;

use crate::App;

pub fn socket_path() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => d.into(),
        None => {
            let temp = std::env::temp_dir();
            warn!(
                "XDG_RUNTIME_DIR is not set. falling back to '{}'",
                temp.display()
            );
            temp
        }
    };

    let display = std::env::var("WAYLAND_DISPLAY")
        .unwrap_or_else(|_| "wayland-0".into())
        .replace("/", "_");

    dir.join(format!("wbg-{}.sock", display))
}

pub fn request(msg: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path())?;

    stream.write_all(msg.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut resp = String::new();

    stream.read_to_string(&mut resp)?;
    Ok(resp)
}

pub fn reply(r: Result<(), image::ImageError>) -> String {
    match r {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!("error: {}\n", e),
    }
}

pub fn handle_client(app: &mut App, mut stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut req = String::new();

    if stream.read_to_string(&mut req).is_err() {
        return;
    };

    let parts: Vec<&str> = req.split('\n').collect();

    let resp = match parts.as_slice() {
        ["get"] => app.describe(),
        ["set", path] => reply(app.set_default(PathBuf::from(path))),
        ["set-output", name, path] => reply(app.set_output(name.to_string(), PathBuf::from(path))),
        ["unset-output", name] => {
            app.unset_output(name);
            "ok\n".to_string()
        }
        _ => "error: bad request\n".to_string(),
    };

    let _ = stream.write_all(resp.as_bytes());
}
