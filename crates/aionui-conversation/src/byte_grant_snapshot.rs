//! Take an immutable, bounded copy of the normal chat attachment target.
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use aionui_ai_agent::protocol::byte_grants::{ByteGrantError, SNAPSHOT_LIMIT};
use aionui_api_types::ChatFileRef;
use aionui_project::{FileOp, ProjectService, ResolvedChatFile};
use same_file::Handle;
use tokio_util::sync::CancellationToken;

pub(crate) async fn snapshot_files(
    project: Arc<ProjectService>,
    owner: String,
    refs: Vec<ChatFileRef>,
    cancel: CancellationToken,
) -> Result<Vec<Vec<u8>>, ByteGrantError> {
    let upload_root = std::env::temp_dir().join("aionui");
    snapshot_inputs(refs, cancel, |file| {
        let project = project.clone();
        let owner = owner.clone();
        let upload_root = upload_root.clone();
        async move {
            project
                .resolve_chat_file(&owner, &file, &upload_root, FileOp::Read)
                .await
                .map_err(|_| ByteGrantError::File)
        }
    })
    .await
}

async fn snapshot_inputs<Fut>(
    refs: Vec<ChatFileRef>,
    cancel: CancellationToken,
    mut resolve: impl FnMut(ChatFileRef) -> Fut,
) -> Result<Vec<Vec<u8>>, ByteGrantError>
where
    Fut: Future<Output = Result<ResolvedChatFile, ByteGrantError>>,
{
    let mut remaining = SNAPSHOT_LIMIT;
    let mut files = Vec::with_capacity(refs.len());
    for file in refs {
        if cancel.is_cancelled() {
            return Err(ByteGrantError::Revoked);
        }
        // Binding::prepare awaits this future after cancellation. Joining both
        // stages keeps the pool reservation alive until their last syscall exits.
        let source = resolve(file).await;
        if cancel.is_cancelled() {
            return Err(ByteGrantError::Revoked);
        }
        let source = source?;
        let token = cancel.clone();
        let result = tokio::task::spawn_blocking(move || match source {
            ResolvedChatFile::Project(path) => snapshot(Path::new(&path), remaining, &token),
            ResolvedChatFile::Attachment(attachment) => {
                attachment.with_file(|file| read_snapshot(file, &file.metadata()?, remaining, &token))
            }
        })
        .await
        .map_err(|_| ByteGrantError::File)?;
        if cancel.is_cancelled() {
            return Err(ByteGrantError::Revoked);
        }
        let bytes = result.map_err(|_| ByteGrantError::File)?;
        remaining -= bytes.len();
        files.push(bytes);
    }
    Ok(files)
}

fn check_cancel(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}

fn open_regular(path: &Path) -> io::Result<File> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    // A regular file raced into a FIFO must not hold a preparation open.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

fn snapshot(path: &Path, remaining: usize, cancel: &CancellationToken) -> io::Result<Vec<u8>> {
    check_cancel(cancel)?;
    let handle = Handle::from_file(open_regular(path)?)?;
    let file = handle.as_file();
    let before = file.metadata()?;
    let bytes = read_snapshot(file, &before, remaining, cancel)?;
    let current = Handle::from_file(open_regular(path)?)?;
    if handle != current
        || !unchanged(&before, &file.metadata()?)?
        || !unchanged(&before, &current.as_file().metadata()?)?
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

fn read_snapshot(
    mut file: &File,
    before: &Metadata,
    remaining: usize,
    cancel: &CancellationToken,
) -> io::Result<Vec<u8>> {
    let length = usize::try_from(before.len()).map_err(|_| io::ErrorKind::InvalidData)?;
    if length > remaining {
        return Err(io::ErrorKind::InvalidData.into());
    }
    check_cancel(cancel)?;
    let mut bytes = vec![0; length];
    for chunk in bytes.chunks_mut(64 * 1024) {
        check_cancel(cancel)?;
        file.read_exact(chunk)?;
    }
    check_cancel(cancel)?;
    if file.read(&mut [0])? != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    check_cancel(cancel)?;
    Ok(bytes)
}

fn unchanged(before: &Metadata, after: &Metadata) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
            return Ok(false);
        }
    }
    Ok(after.is_file() && before.len() == after.len() && before.modified()? == after.modified()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn expired_slots_remain_charged_while_file_validation_is_blocked() {
        use aionui_ai_agent::protocol::{
            acp_relay::RelayWriteAdmission,
            byte_grants::{ByteGrantBinding, ByteGrantPool},
        };
        use std::time::Duration;
        let binding = Arc::new(ByteGrantBinding::default());
        let pool = Arc::new(ByteGrantPool::default());
        let admission = || {
            RelayWriteAdmission::new(None, tokio::time::Instant::now() + Duration::from_secs(60), |write| {
                write()
            })
        };
        let refs = vec![ChatFileRef::Local {
            path: "/fixture".into(),
        }];
        let mut releases = Vec::new();
        // Distinct live bindings isolate the per-owner pool limit from the
        // independent one-pending-batch limit on a single binding.
        let workers = [
            Arc::new(ByteGrantBinding::default()),
            Arc::new(ByteGrantBinding::default()),
        ];
        for (worker, id) in workers.iter().zip(["one", "two"]) {
            let (release, wait) = std::sync::mpsc::channel();
            let (entered, started) = tokio::sync::oneshot::channel();
            let (observed, cancelled) = tokio::sync::oneshot::channel();
            let (done, finished) = tokio::sync::oneshot::channel();
            let batch = refs.clone();
            let pending = worker
                .prepare(
                    &pool,
                    "user",
                    id.into(),
                    &refs,
                    None,
                    admission(),
                    move |token| async move {
                        let observer = token.clone();
                        tokio::spawn(async move {
                            observer.cancelled().await;
                            observed.send(()).unwrap();
                        });
                        let mut gate = Some((entered, wait));
                        let result = snapshot_inputs(batch, token, move |_| {
                            let (entered, wait) = gate.take().unwrap();
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    entered.send(()).unwrap();
                                    wait.recv_timeout(Duration::from_secs(30)).unwrap();
                                    Err(ByteGrantError::File)
                                })
                                .await
                                .unwrap()
                            }
                        })
                        .await;
                        done.send(()).unwrap();
                        result
                    },
                )
                .unwrap();
            started.await.unwrap();
            tokio::time::advance(Duration::from_secs(31)).await;
            assert!(matches!(pending.result().await, Err(ByteGrantError::Expired)));
            cancelled.await.unwrap();
            tokio::task::yield_now().await;
            releases.push((release, finished));
        }
        let reserve = || {
            binding.prepare(&pool, "user", "again".into(), &refs, None, admission(), |_| async {
                Ok(vec![vec![1]])
            })
        };
        assert!(matches!(reserve(), Err(ByteGrantError::Limit)));
        for (release, finished) in releases {
            release.send(()).unwrap();
            finished.await.unwrap();
        }
        assert!(reserve().unwrap().result().await.is_ok());
    }

    #[test]
    fn byte_grant_copy_enforces_the_exact_length_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let cancel = CancellationToken::new();
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(snapshot(&path, 4, &cancel).unwrap(), b"abcd");
        assert_eq!(
            snapshot(&path, 3, &cancel).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            snapshot(dir.path(), 4, &cancel).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            snapshot(&path.with_extension("missing"), 4, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        cancel.cancel();
        assert_eq!(
            snapshot(&path, 4, &cancel).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
