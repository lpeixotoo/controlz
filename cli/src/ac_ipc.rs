//! Local IPC between `controlz ac serve` and `controlz ac status|set`.
//!
//! A Unix socket (`ac.sock` next to `config.toml`, in a 0700 directory; the
//! socket itself is 0600). One request per connection: the client writes
//! its argv joined by tabs plus a newline; the server answers `ok` or `err`
//! on the first line and the rendered text after it, then closes.
//!
//! A socket file whose server does not answer is stale (a serve that was
//! killed) and is replaced; a live one makes a second serve refuse to start.

use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use controlz_core::Config;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::secrets::Error;

/// Longest request line accepted.
const MAX_REQUEST_BYTES: u64 = 4096;
/// Longest reply accepted by the client.
const MAX_REPLY_BYTES: u64 = 64 * 1024;
/// For a client to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// For connecting to a socket (also the liveness probe).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// For the whole reply: a `set` waits for up to three ACKs and a report.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// `ac.sock` next to the config file.
pub fn default_socket_path() -> Result<PathBuf, Error> {
    let config = Config::default_path()?;
    let dir = config.parent().ok_or("config path has no directory")?;
    Ok(dir.join("ac.sock"))
}

/// Binds the server socket at `path`: creates its directory (0700, and
/// tightens an existing one to 0700), replaces a stale socket, and refuses
/// if another server answers there.
pub async fn bind(path: &Path) -> Result<UnixListener, Error> {
    let dir = path.parent().ok_or("socket path has no directory")?;
    fs::create_dir_all(dir)?;
    let mode = fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        // The full path holds the macOS user name; the file name is enough.
        tracing::info!("ipc: socket directory restricted to 0700");
    }
    if fs::symlink_metadata(path).is_ok() {
        if connect(path).await.is_ok() {
            return Err("another `controlz ac serve` is already running".into());
        }
        tracing::info!(socket = %file_name(path), "ipc: removing a stale socket");
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The socket's file name (`ac.sock`), for logs.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

async fn connect(path: &Path) -> std::io::Result<UnixStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))?
}

/// Serves one connection: reads the request line, runs `handler` on its
/// arguments and writes the reply. Bad requests get an `err` reply.
pub async fn serve_one<F, Fut>(stream: UnixStream, handler: F)
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let mut limited = BufReader::new(reader.take(MAX_REQUEST_BYTES + 1));
    let read = tokio::time::timeout(REQUEST_TIMEOUT, limited.read_line(&mut line)).await;
    let reply = match read {
        Ok(Ok(_)) if line.ends_with('\n') => {
            let args = line.trim_end_matches('\n').split('\t').map(str::to_owned);
            handler(args.collect()).await
        }
        Ok(Ok(_)) => Err("request line missing or too long".to_owned()),
        Ok(Err(e)) => Err(format!("request unreadable: {e}")),
        Err(_) => Err("request timed out".to_owned()),
    };
    let text = match reply {
        Ok(text) => format!("ok\n{text}"),
        Err(text) => format!("err\n{text}"),
    };
    // The client may have gone away; nothing to do about it.
    let _ = writer.write_all(text.as_bytes()).await;
    let _ = writer.shutdown().await;
}

/// Sends `args` to the server at `path` and returns its reply text.
///
/// # Errors
/// If no server answers, an argument contains a tab or newline, the reply
/// is late or malformed, or the server answered `err` (its text).
pub async fn request(path: &Path, args: &[String]) -> Result<String, Error> {
    if args.iter().any(|a| a.contains(['\t', '\n'])) {
        return Err("arguments must not contain tabs or newlines".into());
    }
    let mut stream = connect(path)
        .await
        .map_err(|_| "`controlz ac serve` is not running (start it in another terminal)")?;
    stream
        .write_all(format!("{}\n", args.join("\t")).as_bytes())
        .await?;
    let mut raw = String::new();
    let mut limited = stream.take(MAX_REPLY_BYTES);
    tokio::time::timeout(REPLY_TIMEOUT, limited.read_to_string(&mut raw))
        .await
        .map_err(|_| "no reply from `controlz ac serve` in time")??;
    match raw.split_once('\n') {
        Some(("ok", text)) => Ok(text.to_owned()),
        Some(("err", text)) => Err(text.trim_end().to_owned().into()),
        _ => Err("malformed reply from `controlz ac serve`".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short, unique directory (Unix socket paths are limited to 104 bytes).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cz-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn echo_server(listener: UnixListener, connections: usize) {
        for _ in 0..connections {
            let (stream, _) = listener.accept().await.unwrap();
            serve_one(stream, |args: Vec<String>| async move {
                match args.first().map(String::as_str) {
                    Some("fail") => Err("it failed".to_owned()),
                    _ => Ok(format!("got {}\n", args.join("|"))),
                }
            })
            .await;
        }
    }

    #[tokio::test]
    async fn round_trip_and_errors() {
        let dir = TempDir::new("rt");
        let path = dir.0.join("sub").join("ac.sock");
        let listener = bind(&path).await.unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let server = tokio::spawn(echo_server(listener, 2));

        let args = ["set", "--power", "on"].map(str::to_owned);
        assert_eq!(request(&path, &args).await.unwrap(), "got set|--power|on\n");
        let err = request(&path, &["fail".to_owned()]).await.unwrap_err();
        assert_eq!(err.to_string(), "it failed");
        let err = request(&path, &["a\tb".to_owned()]).await.unwrap_err();
        assert!(err.to_string().contains("tabs"), "{err}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_and_live_one_refuses() {
        let dir = TempDir::new("stale");
        fs::create_dir_all(&dir.0).unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.0.join("ac.sock");
        // A socket file nobody listens on, as a killed serve leaves behind.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let err = request(&path, &["status".to_owned()]).await.unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");

        let listener = bind(&path).await.expect("a stale socket is replaced");
        assert_eq!(
            fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
            0o700,
            "directory tightened"
        );
        let err = bind(&path).await.unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
        drop(listener);
    }

    #[tokio::test]
    async fn oversized_or_unterminated_requests_get_err() {
        let dir = TempDir::new("big");
        let path = dir.0.join("ac.sock");
        let listener = bind(&path).await.unwrap();
        let server = tokio::spawn(echo_server(listener, 1));
        let mut stream = UnixStream::connect(&path).await.unwrap();
        let big = vec![b'a'; usize::try_from(MAX_REQUEST_BYTES).unwrap() + 10];
        stream.write_all(&big).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "err\nrequest line missing or too long");
        server.await.unwrap();
    }
}
