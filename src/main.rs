use crate::iface::Iface;
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::future::Future;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use mio::{Evented, Token, Ready, PollOpt};
use mio::unix::EventedFd;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, thread};
use tokio::io::PollEvented;
use tokio::runtime::Runtime;

mod iface;

pub fn unshare_user() -> Result<(), io::Error> {
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    unsafe { errno!(libc::unshare(libc::CLONE_NEWUSER))? };

    let mut f = File::create("/proc/self/uid_map")?;
    let s = format!("0 {} 1\n", uid);
    f.write(s.as_bytes())?;

    let mut f = File::create("/proc/self/setgroups")?;
    f.write(b"deny\n")?;

    let mut f = File::create("/proc/self/gid_map")?;
    let s = format!("0 {} 1\n", gid);
    f.write(s.as_bytes())?;

    Ok(())
}

pub struct TokioFd(Iface);

impl TokioFd {
    pub fn new(iface: Iface) -> Result<Self, ()> {
        Ok(Self(iface))
    }
}

/*impl Evented for Iface {
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt
    ) -> io::Result<()> {
        let fd = self.as_raw_fd();
        let evented_fd = EventedFd(&fd);
        evented_fd.register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt
    ) -> io::Result<()> {
        let fd = self.as_raw_fd();
        let evented_fd = EventedFd(&fd);
        evented_fd.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        let fd = self.as_raw_fd();
        let evented_fd = EventedFd(&fd);
        evented_fd.deregister(poll)
    }
}*/

impl AsyncRead for TokioFd {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<Result<usize, io::Error>> {
        Poll::Ready(self.0.read(buf))
    }
}

impl AsyncWrite for TokioFd {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        Poll::Ready(self.0.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Spawns a thread in a new network namespace and configures a TUN interface that sends and
/// receives IP packets from the tx/rx channels and runs some UDP/TCP networking code in task.
pub fn machine<F>(
    addr: Ipv4Addr,
    mask: u8,
    mut tx: Sender<Vec<u8>>,
    mut rx: Receiver<Vec<u8>>,
    task: F,
) -> thread::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    thread::spawn(move || {
        unsafe {
            errno!(libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWUTS)).unwrap();
        }
        let iface = Iface::new().unwrap();
        iface.set_ipv4_addr(addr, mask).unwrap();
        iface.put_up().unwrap();
        let (mut reader, mut writer) = TokioFd::new(iface).unwrap().split();
        let mut rt = tokio::runtime::Builder::new()
            .threaded_scheduler()
            .enable_io()
            .build()
            .unwrap();

        rt.block_on(async move {
            tokio::spawn(async move {
                loop {
                    let mut buf = [0; libc::ETH_FRAME_LEN as usize];
                    let n = reader.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            });

            tokio::spawn(async move {
                loop {
                    if let Some(packet) = rx.next().await {
                        let n = writer.write(&packet).await.unwrap();
                        if n == 0 {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            });

            task.await
        })
    })
}

fn main() {
    use tokio::net::UdpSocket;

    unshare_user().unwrap();
    let a_addr: Ipv4Addr = "192.168.1.5".parse().unwrap();
    let b_addr = "192.168.1.6".parse().unwrap();
    let (a_tx, b_rx) = mpsc::channel(0);
    let (b_tx, a_rx) = mpsc::channel(0);

    let join1 = machine(a_addr.clone(), 24, a_tx, a_rx, async move {
        let mut socket = UdpSocket::bind(SocketAddrV4::new(0.into(), 3000)).await.unwrap();
        loop {
            let mut buf = [0u8; 11];
            let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
            if &buf[..len] == b"ping" {
                println!("received ping");
                socket.send_to(b"pong", addr).await.unwrap();
                break;
            }
        }
    });

    let join2 = machine(b_addr, 24, b_tx, b_rx, async move {
        let mut socket = UdpSocket::bind(SocketAddrV4::new(0.into(), 3000)).await.unwrap();
        socket
            .send_to(b"ping", SocketAddrV4::new(a_addr, 3000))
            .await
            .unwrap();

        let mut buf = [0u8; 11];
        let (len, _addr) = socket.recv_from(&mut buf).await.unwrap();
        if &buf[..len] == b"pong" {
            println!("received pong");
        }
    });

    join1.join().unwrap();
    join2.join().unwrap();
}
