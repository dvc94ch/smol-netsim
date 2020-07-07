use crate::iface::Iface;
use std::fs::File;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::{io, thread};

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

pub fn machine<F, R>(
    addr: Ipv4Addr,
    mask: u8,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    task: F,
) -> thread::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    thread::spawn(move || {
        unsafe {
            errno!(libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWUTS)).unwrap();
        }
        let iface = Arc::new(Iface::new().unwrap());
        iface.set_ipv4_addr(addr, mask).unwrap();
        iface.put_up().unwrap();
        let iface2 = iface.clone();

        thread::spawn(move || loop {
            let mut buf = [0; libc::ETH_FRAME_LEN as usize];
            let n = iface.recv(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        });

        thread::spawn(move || loop {
            if let Ok(packet) = rx.recv() {
                let n = iface2.send(&packet).unwrap();
                if n == 0 {
                    break;
                }
            } else {
                break;
            }
        });

        task()
    })
}

fn main() {
    unshare_user().unwrap();
    let a_addr: Ipv4Addr = "192.168.1.5".parse().unwrap();
    let b_addr = "192.168.1.6".parse().unwrap();
    let (a_tx, b_rx) = mpsc::channel();
    let (b_tx, a_rx) = mpsc::channel();

    let join1 = machine(a_addr.clone(), 24, a_tx, a_rx, move || {
        let socket = UdpSocket::bind("0.0.0.0:3000").unwrap();
        loop {
            let mut buf = [0u8; 11];
            let (len, addr) = socket.recv_from(&mut buf).unwrap();
            if &buf[..len] == b"ping" {
                println!("received ping");
                socket.send_to(b"pong", addr).unwrap();
                break;
            }
        }
    });

    let join2 = machine(b_addr, 24, b_tx, b_rx, move || {
        let socket = UdpSocket::bind("0.0.0.0:3000").unwrap();
        socket
            .send_to(b"ping", SocketAddrV4::new(a_addr, 3000))
            .unwrap();

        let mut buf = [0u8; 11];
        let (len, _addr) = socket.recv_from(&mut buf).unwrap();
        if &buf[..len] == b"pong" {
            println!("received pong");
        }
    });

    join1.join().unwrap();
    join2.join().unwrap();
}
