//! Job-owned immutable source and staging sink. No path-bearing renderer DTOs.
use super::contract::*;
use std::{
    path::{Path, PathBuf},
    pin::Pin,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, ReadBuf};

fn io_error(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
pub(crate) struct SpoolSource {
    path: PathBuf,
    length: u64,
}
impl SpoolSource {
    /// Only a native owner holding the capture's lifetime may construct this.
    /// Verify again after reopening a journal or recovering a process.
    pub fn verified(path: &Path, length: u64, sha256: &str) -> Result<Self> {
        verify_file(path, length, sha256)?;
        Ok(Self {
            path: path.into(),
            length,
        })
    }
}
fn verify_file(path: &Path, length: u64, sha256: &str) -> Result<()> {
    let mut file = crate::trust_boundary::open_regular_source(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() != length {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let hash = risunest_external_storage_format::content_identity::hash_reader(&mut file, length)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    if hex::encode(hash) != sha256 {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(())
}
impl TransferSource for SpoolSource {
    fn byte_length(&self) -> u64 {
        self.length
    }
    fn open<'a>(
        &'a self,
        offset: u64,
        length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncRead + Send>>> {
        Box::pin(async move {
            cancel.check()?;
            if offset
                .checked_add(length)
                .is_none_or(|end| end > self.length)
            {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let file = crate::trust_boundary::open_regular_source(&self.path).map_err(io_error)?;
            if file.metadata().map_err(io_error)?.len() != self.length {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let mut file = tokio::fs::File::from_std(file);
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(io_error)?;
            Ok(Box::pin(CancelledReader {
                reader: file.take(length),
                cancel: cancel.clone(),
            }) as Pin<Box<dyn AsyncRead + Send>>)
        })
    }
}

struct CancelledReader {
    reader: tokio::io::Take<tokio::fs::File>,
    cancel: Cancellation,
}
impl AsyncRead for CancelledReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.cancel.check().is_err() {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "cancelled",
            )));
        }
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

pub(crate) struct SpoolSink {
    path: PathBuf,
    max_length: u64,
    verified: bool,
}
impl SpoolSink {
    pub fn create(path: &Path, max_length: u64) -> Result<Self> {
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(io_error)?;
        Ok(Self {
            path: path.into(),
            max_length,
            verified: false,
        })
    }
    pub fn is_verified(&self) -> bool {
        self.verified
    }
}
/// The writer refuses a provider overrun before it reaches the staging file.
struct LimitedWriter {
    file: tokio::fs::File,
    remaining: u64,
    cancel: Cancellation,
}
impl AsyncWrite for LimitedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.cancel.check().is_err() {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "cancelled",
            )));
        }
        if bytes.len() as u64 > self.remaining {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "transfer-limit-exceeded",
            )));
        }
        match Pin::new(&mut self.file).poll_write(cx, bytes) {
            std::task::Poll::Ready(Ok(written)) => {
                self.remaining -= written as u64;
                std::task::Poll::Ready(Ok(written))
            }
            other => other,
        }
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_shutdown(cx)
    }
}
impl TransferSink for SpoolSink {
    fn open<'a>(
        &'a mut self,
        offset: u64,
        max_length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncWrite + Send>>> {
        Box::pin(async move {
            cancel.check()?;
            if self.verified
                || offset
                    .checked_add(max_length)
                    .is_none_or(|end| end > self.max_length)
            {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .await
                .map_err(io_error)?;
            if file.metadata().await.map_err(io_error)?.len() < offset {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            file.set_len(offset).await.map_err(io_error)?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(io_error)?;
            Ok(Box::pin(LimitedWriter {
                file,
                remaining: max_length,
                cancel: cancel.clone(),
            }) as Pin<Box<dyn AsyncWrite + Send>>)
        })
    }
    fn finish<'a>(&'a mut self, length: u64, sha256: &'a str) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            if self.verified || length > self.max_length {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let path = self.path.clone();
            let sha256 = sha256.to_owned();
            tokio::task::spawn_blocking(move || {
                verify_file(&path, length, &sha256)?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(io_error)?
                    .sync_all()
                    .map_err(io_error)
            })
            .await
            .map_err(io_error)??;
            self.verified = true;
            Ok(())
        })
    }
}
