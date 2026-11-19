//! Child stdin whose Windows write receipt marks a completed pipe syscall.
use std::io::{self, PipeReader, PipeWriter, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::time::Sleep;

const PIPE_NOWAIT: u32 = 1;
const PIPE_RETRY: Duration = Duration::from_millis(5);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetNamedPipeHandleState(
        pipe: RawHandle,
        mode: *const u32,
        max_collection_count: *mut u32,
        collection_timeout: *mut u32,
    ) -> i32;
}

pub(super) struct DirectStdin {
    writer: PipeWriter,
    retry: Option<Pin<Box<Sleep>>>,
}

impl DirectStdin {
    pub(super) fn pipe() -> io::Result<(PipeReader, Self)> {
        let (reader, writer) = io::pipe()?;
        // SAFETY: writer owns the live pipe handle, mode is readable, and
        // the unused collection parameters are null. Only the writer's mode
        // changes. The child gets the ordinary blocking read end.
        let success =
            unsafe { SetNamedPipeHandleState(writer.as_raw_handle(), &PIPE_NOWAIT, ptr::null_mut(), ptr::null_mut()) };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((reader, Self { writer, retry: None }))
    }
}

impl AsyncWrite for DirectStdin {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Some(retry) = &mut self.retry {
            ready!(retry.as_mut().poll(cx));
            self.retry = None;
        }
        // Microsoft documents that synchronous WriteFile on a local byte
        // pipe in PIPE_NOWAIT returns the bytes accepted by the pipe, or
        // zero when full. There is no later worker that can send the frame.
        match self.writer.write(bytes) {
            Ok(0) => {
                let mut retry = Box::pin(tokio::time::sleep(PIPE_RETRY));
                let _ = retry.as_mut().poll(cx);
                self.retry = Some(retry);
                Poll::Pending
            }
            result => Poll::Ready(result),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // No userspace buffer or outstanding write. FlushFileBuffers would
        // wait for the child to read and would change the transport contract.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use tokio::io::AsyncWriteExt as _;

    #[tokio::test]
    async fn acp_relay_windows_pipe_reports_actual_write_and_closed_reader() {
        let (mut reader, mut writer) = DirectStdin::pipe().unwrap();
        writer.write_all(b"first\n").await.unwrap();
        let mut received = [0; 6];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"first\n");

        // Independent output can stay open while stdin fails.
        let (_stdout_reader, _stdout_writer) = io::pipe().unwrap();
        drop(reader);
        assert!(writer.write_all(b"next\n").await.is_err());
    }
}
