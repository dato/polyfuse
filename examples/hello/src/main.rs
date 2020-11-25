#![allow(clippy::unnecessary_mut_passed)]
#![deny(clippy::unimplemented)]

use polyfuse::{
    op,
    reply::{self, EntryOptions},
    Config, Operation, Session,
};
use polyfuse_async_std::Connection;

use anyhow::Context as _;
use std::{mem, os::unix::prelude::*, path::PathBuf, time::Duration};

const TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365);
const ROOT_INO: u64 = 1;
const HELLO_INO: u64 = 2;
const HELLO_FILENAME: &str = "hello.txt";
const HELLO_CONTENT: &[u8] = b"Hello, world!\n";

#[async_std::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = pico_args::Arguments::from_env();

    let mountpoint: PathBuf = args
        .free_from_str()?
        .context("missing mountpoint specified")?;
    anyhow::ensure!(mountpoint.is_dir(), "the mountpoint must be a directory");

    let conn = Connection::open(&mountpoint, &[]).await?;

    let session = Session::start(&conn, Config::default()).await?;

    let fs = Hello::new();

    while let Some(req) = session.next_request(&conn).await? {
        req.process(&conn, |op| async {
            match op {
                Operation::Lookup { op, reply, .. } => fs.lookup(op, reply).await,
                Operation::Getattr { op, reply, .. } => fs.getattr(op, reply).await,
                Operation::Read { op, reply, .. } => fs.read(op, reply).await,
                Operation::Readdir { op, reply, .. } => fs.readdir(op, reply).await,

                _ => Err(polyfuse::reply::error_code(libc::ENOSYS)),
            }
        })
        .await?;
    }

    Ok(())
}

struct Hello {
    root_attr: libc::stat,
    hello_attr: libc::stat,
    entries: Vec<DirEntry>,
}

struct DirEntry {
    name: &'static str,
    ino: u64,
    typ: u32,
}

impl Hello {
    fn new() -> Self {
        let root_attr = root_attr();
        let hello_attr = hello_attr();

        let mut entries = Vec::with_capacity(3);
        entries.push(DirEntry {
            name: ".",
            ino: ROOT_INO,
            typ: libc::DT_DIR as u32,
        });
        entries.push(DirEntry {
            name: "..",
            ino: ROOT_INO,
            typ: libc::DT_DIR as u32,
        });
        entries.push(DirEntry {
            name: HELLO_FILENAME,
            ino: HELLO_INO,
            typ: libc::DT_REG as u32,
        });

        Self {
            root_attr,
            hello_attr,
            entries,
        }
    }

    async fn lookup<Op, R>(&self, op: Op, reply: R) -> Result<R::Ok, R::Error>
    where
        Op: op::Lookup,
        R: reply::ReplyEntry,
    {
        match op.parent() {
            ROOT_INO if op.name().as_bytes() == HELLO_FILENAME.as_bytes() => reply.entry(
                &self.hello_attr,
                &EntryOptions {
                    ino: HELLO_INO,
                    ttl_attr: Some(TTL),
                    ttl_entry: Some(TTL),
                    ..Default::default()
                },
            ),
            _ => Err(polyfuse::reply::error_code(libc::ENOENT)),
        }
    }

    async fn getattr<Op, R>(&self, op: Op, reply: R) -> Result<R::Ok, R::Error>
    where
        Op: op::Getattr,
        R: reply::ReplyAttr,
    {
        let attr = match op.ino() {
            ROOT_INO => &self.root_attr,
            HELLO_INO => &self.hello_attr,
            _ => return Err(polyfuse::reply::error_code(libc::ENOENT)),
        };
        reply.attr(attr, Some(TTL))
    }

    async fn read<Op, R>(&self, op: Op, reply: R) -> Result<R::Ok, R::Error>
    where
        Op: op::Read,
        R: reply::ReplyData,
    {
        match op.ino() {
            ROOT_INO => return Err(polyfuse::reply::error_code(libc::EISDIR)),
            HELLO_INO => (),
            _ => return Err(polyfuse::reply::error_code(libc::ENOENT)),
        }

        let offset = op.offset() as usize;
        if offset >= HELLO_CONTENT.len() {
            return reply.data(&[]);
        }

        let size = op.size() as usize;
        let data = &HELLO_CONTENT[offset..];
        let data = &data[..std::cmp::min(data.len(), size)];

        reply.data(data)
    }

    fn dir_entries(&self) -> impl Iterator<Item = (u64, &DirEntry)> + '_ {
        self.entries.iter().enumerate().map(|(i, ent)| {
            let offset = (i + 1) as u64;
            (offset, ent)
        })
    }

    async fn readdir<Op, R>(&self, op: Op, mut reply: R) -> Result<R::Ok, R::Error>
    where
        Op: op::Readdir,
        R: reply::ReplyDirs,
    {
        if op.ino() != ROOT_INO {
            return Err(polyfuse::reply::error_code(libc::ENOTDIR));
        }

        for (i, entry) in self.dir_entries().skip(op.offset() as usize) {
            let full = reply.add(entry.ino, entry.typ, entry.name.as_ref(), i + 1);
            if full {
                break;
            }
        }

        reply.send()
    }
}

fn root_attr() -> libc::stat {
    let mut attr = unsafe { mem::zeroed::<libc::stat>() };
    attr.st_mode = libc::S_IFDIR as u32 | 0o555;
    attr.st_ino = ROOT_INO;
    attr.st_nlink = 2;
    attr.st_uid = unsafe { libc::getuid() };
    attr.st_gid = unsafe { libc::getgid() };
    attr
}

fn hello_attr() -> libc::stat {
    let mut attr = unsafe { mem::zeroed::<libc::stat>() };
    attr.st_size = HELLO_CONTENT.len() as libc::off_t;
    attr.st_mode = libc::S_IFREG as u32 | 0o444;
    attr.st_ino = HELLO_INO;
    attr.st_nlink = 1;
    attr.st_uid = unsafe { libc::getuid() };
    attr.st_gid = unsafe { libc::getgid() };
    attr
}
