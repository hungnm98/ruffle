//! Native AMF transport. Each connection owns a bundled librtmp worker.
//! Credentials travel only in framed stdin/stdout messages, never worker arguments.
use async_channel::{Receiver, Sender};
use futures_lite::FutureExt;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use url::Url;

const MAX_FRAME: usize = 8 * 1024 * 1024;

pub(super) fn destination(value: &str) -> Option<(String, u16)> {
    if value.len() > 2048 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "rtmp" | "rtmpe")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().trim_matches('/').is_empty()
    {
        return None;
    }
    Some((url.host_str()?.to_owned(), url.port().unwrap_or(1935)))
}

pub(super) fn worker_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    Ok(executable
        .parent()
        .ok_or_else(|| io::Error::from(ErrorKind::NotFound))?
        .join(if cfg!(windows) {
            "rtmp-worker.exe"
        } else {
            "rtmp-worker"
        }))
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(ErrorKind::InvalidData.into());
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

pub(super) async fn exchange(
    worker: &Path,
    url: &str,
    receiver: Receiver<Vec<u8>>,
    sender: Sender<Vec<u8>>,
) -> io::Result<()> {
    if destination(url).is_none() {
        return Err(ErrorKind::InvalidInput.into());
    }
    let mut command = Command::new(worker);
    command
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");

    let read = async {
        // Bound the handshake, but leave established game sessions free of an idle deadline.
        let first = read_frame(&mut stdout)
            .or(async {
                async_io::Timer::after(Duration::from_secs(20)).await;
                Err(ErrorKind::TimedOut.into())
            })
            .await?;
        sender
            .try_send(first)
            .map_err(|_| io::Error::from(ErrorKind::BrokenPipe))?;
        loop {
            let bytes = read_frame(&mut stdout).await?;
            if sender.try_send(bytes).is_err() {
                break Err::<(), io::Error>(ErrorKind::BrokenPipe.into());
            }
        }
    };
    let write = async {
        while let Ok(bytes) = receiver.recv().await {
            write_frame(&mut stdin, &bytes).await?;
        }
        Ok::<(), io::Error>(())
    };
    // Race entire loops, not individual read_exact calls: a partial frame must
    // survive an outgoing message. Closing the core channel ends both loops.
    let result = read.or(write).await;
    drop(stdin);
    drop(stdout);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_destination_without_librtmp_options_or_credentials() {
        assert_eq!(
            destination("rtmpe://103.116.100.95:80/master/test/"),
            Some(("103.116.100.95".into(), 80))
        );
        assert_eq!(
            destination("rtmp://localhost/app"),
            Some(("localhost".into(), 1935))
        );
        for value in [
            "https://host/app",
            "rtmpe://user:pass@host/app",
            "rtmpe://host/app?secret=1",
            "rtmpe://host/app#secret",
            "rtmpe://host/",
            "rtmpe://host/app live=1",
            "rtmpe://host/app\n",
        ] {
            assert!(destination(value).is_none(), "{value}");
        }
    }

    #[tokio::test]
    async fn frames_handle_split_reads_and_binary_payloads() {
        let (mut writer, mut reader) = tokio::io::duplex(3);
        let send = async {
            write_frame(&mut writer, &[0, 255, 128, 10, 13])
                .await
                .unwrap();
            write_frame(&mut writer, &[42]).await.unwrap();
        };
        let receive = async {
            assert_eq!(
                read_frame(&mut reader).await.unwrap(),
                [0, 255, 128, 10, 13]
            );
            assert_eq!(read_frame(&mut reader).await.unwrap(), [42]);
        };
        tokio::join!(send, receive);
    }

    #[tokio::test]
    async fn rejects_invalid_or_truncated_frames() {
        for length in [0u32, MAX_FRAME as u32 + 1] {
            assert_eq!(
                read_frame(&mut &length.to_be_bytes()[..])
                    .await
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidData
            );
        }
        assert_eq!(
            read_frame(&mut &[0, 0, 0, 2, 1][..])
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::UnexpectedEof
        );
        assert_eq!(
            write_frame(&mut Vec::new(), &[]).await.unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_exit_closes_channels() {
        let (outgoing, receiver) = async_channel::bounded(1);
        let (sender, incoming) = async_channel::bounded(1);
        let result = exchange(
            Path::new("/usr/bin/true"),
            "rtmp://localhost/app",
            receiver,
            sender,
        )
        .await;
        assert!(result.is_err());
        assert!(incoming.recv().await.is_err());
        assert!(outgoing.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bidirectional_worker_stops_when_core_closes() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("echo-worker");
        std::fs::write(&worker, "#!/bin/sh\nexec /bin/cat\n").unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (outgoing, receiver) = async_channel::bounded(1);
        let (sender, incoming) = async_channel::bounded(1);
        let task = tokio::spawn(async move {
            exchange(&worker, "rtmp://localhost/app", receiver, sender).await
        });
        outgoing.send(vec![0, 255, 13, 10]).await.unwrap();
        assert_eq!(incoming.recv().await.unwrap(), [0, 255, 13, 10]);
        outgoing.send(vec![1, 2, 3]).await.unwrap();
        assert_eq!(incoming.recv().await.unwrap(), [1, 2, 3]);
        drop(outgoing);
        task.await.unwrap().unwrap();
        assert!(incoming.recv().await.is_err());
    }
}
