use crate::{
    abi::InitOut, //
    buffer::Buffer,
    op::Operations,
    reply::ReplyRaw,
    request::Arg,
    MAX_WRITE_SIZE,
};
use futures::{
    future::{FusedFuture, FutureExt},
    io::{AsyncRead, AsyncWrite},
    select,
};
use std::{io, pin::Pin};

/// A single threaded, sequential filesystem driver.
///
/// This session driver does *not* receive the next request from the kernel,
/// until the processing result for the previous request has been sent.
#[derive(Debug)]
pub struct Session {
    proto_major: Option<u32>,
    proto_minor: Option<u32>,
    got_init: bool,
    got_destroy: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// Create a new `Session`.
    pub fn new() -> Self {
        Self {
            proto_major: None,
            proto_minor: None,
            got_init: false,
            got_destroy: false,
        }
    }

    /// Returns the major protocol version negotiated with the kernel.
    pub fn proto_major(&self) -> Option<u32> {
        self.proto_major
    }

    /// Returns the minor protocol version negotiated with the kernel.
    pub fn proto_minor(&self) -> Option<u32> {
        self.proto_minor
    }

    /// Run a session with the kernel.
    #[cfg(feature = "tokio")]
    pub async fn run<'a, I, T>(
        &'a mut self,
        io: &'a mut I,
        buf: &'a mut Buffer,
        ops: &'a mut T,
    ) -> io::Result<()>
    where
        I: AsyncRead + AsyncWrite + Unpin,
        T: Operations,
    {
        self.run_until(io, buf, ops, crate::tokio::default_shutdown_signal()?)
            .await?;
        Ok(())
    }

    /// Run a session with the kernel until the provided shutdown signal is received.
    pub async fn run_until<'a, I, T, S>(
        &'a mut self,
        io: &'a mut I,
        buf: &'a mut Buffer,
        ops: &'a mut T,
        sig: S,
    ) -> io::Result<Option<S::Output>>
    where
        I: AsyncRead + AsyncWrite + Unpin,
        T: Operations,
        S: FusedFuture + Unpin,
    {
        let mut sig = sig;
        loop {
            let mut turn = self.turn(io, buf, ops);
            let mut turn = unsafe { Pin::new_unchecked(&mut turn) }.fuse();

            select! {
                res = turn => {
                    let destroyed = res?;
                    if destroyed {
                        log::debug!("The session is closed successfully");
                        return Ok(None);
                    }
                },
                res = sig => {
                    log::debug!("Got shutdown signal");
                    return Ok(Some(res))
                },
            };
        }
    }

    /// Receives one request from the channel, and returns its processing result.
    #[allow(clippy::cognitive_complexity)]
    pub async fn turn<'a, I, T>(
        &'a mut self,
        io: &'a mut I,
        buf: &'a mut Buffer,
        ops: &'a mut T,
    ) -> io::Result<bool>
    where
        I: AsyncRead + AsyncWrite + Unpin,
        T: Operations,
    {
        if self.got_destroy {
            return Ok(true);
        }

        let (header, arg, data) = match buf.receive(io).await? {
            Some(res) => res,
            None => {
                return Ok(true);
            }
        };

        log::debug!(
            "Got a request: unique={}, opcode={:?}, arg={:?}, data={:?}",
            header.unique,
            header.opcode(),
            arg,
            data
        );

        let reply = ReplyRaw::new(header.unique, &mut *io);
        match arg {
            Arg::Init(arg) => {
                let mut init_out = InitOut::default();

                if arg.major > 7 {
                    log::debug!("wait for a second INIT request with a 7.X version.");
                    reply.reply(0, &[init_out.as_ref()]).await?;
                    return Ok(false);
                }

                if arg.major < 7 || (arg.major == 7 && arg.minor < 6) {
                    log::warn!("unsupported protocol version: {}.{}", arg.major, arg.minor);
                    reply.reply_err(libc::EPROTO).await?;
                    return Err(io::Error::from_raw_os_error(libc::EPROTO));
                }

                // remember the protocol version
                self.proto_major = Some(arg.major);
                self.proto_minor = Some(arg.minor);

                // TODO: max_background, congestion_threshold, time_gran, max_pages
                init_out.max_readahead = arg.max_readahead;
                init_out.max_write = MAX_WRITE_SIZE as u32;

                self.got_init = true;
                reply.reply(0, &[init_out.as_ref()]).await?;
            }
            _ if !self.got_init => {
                log::warn!(
                    "ignoring an operation before init (opcode={:?})",
                    header.opcode
                );
                reply.reply_err(libc::EIO).await?;
                return Ok(false);
            }
            Arg::Destroy => {
                self.got_destroy = true;
                reply.reply(0, &[]).await?;
            }
            _ if self.got_destroy => {
                log::warn!(
                    "ignoring an operation after destroy (opcode={:?})",
                    header.opcode
                );
                reply.reply_err(libc::EIO).await?;
                return Ok(false);
            }
            Arg::Lookup { name } => ops.lookup(header, name, reply.into()).await?,
            Arg::Forget(arg) => {
                ops.forget(header, arg);
            }
            Arg::Getattr(arg) => ops.getattr(header, &arg, reply.into()).await?,
            Arg::Setattr(arg) => ops.setattr(header, &arg, reply.into()).await?,
            Arg::Readlink => ops.readlink(header, reply.into()).await?,
            Arg::Symlink { name, link } => ops.symlink(header, name, link, reply.into()).await?,
            Arg::Mknod { arg, name } => ops.mknod(header, arg, name, reply.into()).await?,
            Arg::Mkdir { arg, name } => ops.mkdir(header, arg, name, reply.into()).await?,
            Arg::Unlink { name } => ops.unlink(header, name, reply.into()).await?,
            Arg::Rmdir { name } => ops.unlink(header, name, reply.into()).await?,
            Arg::Rename { arg, name, newname } => {
                ops.rename(header, arg, name, newname, reply.into()).await?
            }
            Arg::Link { arg, newname } => ops.link(header, arg, newname, reply.into()).await?,
            Arg::Open(arg) => ops.open(header, arg, reply.into()).await?,
            Arg::Read(arg) => ops.read(header, arg, reply.into()).await?,
            Arg::Write(arg) => match data {
                Some(data) => ops.write(header, arg, data, reply.into()).await?,
                None => panic!("unexpected condition"),
            },
            Arg::Release(arg) => ops.release(header, arg, reply.into()).await?,
            Arg::Statfs => ops.statfs(header, reply.into()).await?,
            Arg::Fsync(arg) => ops.fsync(header, arg, reply.into()).await?,
            Arg::Setxattr { arg, name, value } => {
                ops.setxattr(header, arg, name, value, reply.into()).await?
            }
            Arg::Getxattr { arg, name } => ops.getxattr(header, arg, name, reply.into()).await?,
            Arg::Listxattr { arg } => ops.listxattr(header, arg, reply.into()).await?,
            Arg::Removexattr { name } => ops.removexattr(header, name, reply.into()).await?,
            Arg::Flush(arg) => ops.flush(header, arg, reply.into()).await?,
            Arg::Opendir(arg) => ops.opendir(header, arg, reply.into()).await?,
            Arg::Readdir(arg) => ops.readdir(header, arg, reply.into()).await?,
            Arg::Releasedir(arg) => ops.releasedir(header, arg, reply.into()).await?,
            Arg::Fsyncdir(arg) => ops.fsyncdir(header, arg, reply.into()).await?,
            Arg::Getlk(arg) => ops.getlk(header, arg, reply.into()).await?,
            Arg::Setlk(arg) => ops.setlk(header, arg, false, reply.into()).await?,
            Arg::Setlkw(arg) => ops.setlk(header, arg, true, reply.into()).await?,
            Arg::Access(arg) => ops.access(header, arg, reply.into()).await?,
            Arg::Create(arg) => ops.create(header, arg, reply.into()).await?,
            Arg::Bmap(arg) => ops.bmap(header, arg, reply.into()).await?,

            // Interrupt,
            // Ioctl,
            // Poll,
            // NotifyReply,
            // BatchForget,
            // Fallocate,
            // Readdirplus,
            // Rename2,
            // Lseek,
            // CopyFileRange,
            Arg::Unknown => {
                log::warn!("unsupported opcode: {:?}", header.opcode);
                reply.reply_err(libc::ENOSYS).await?;
            }
        }

        Ok(false)
    }
}
