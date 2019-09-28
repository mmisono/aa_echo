// Synchronous echo server.
// Handle only one connection at a time.

use std::io;
use std::mem;
use std::os::unix::io::RawFd;

#[macro_use]
mod util;

struct Ipv4Addr(libc::in_addr);
struct TcpListener(RawFd);
struct TcpStream(RawFd);
struct Incoming<'a> {
    listener: &'a TcpListener,
}

impl Ipv4Addr {
    pub fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Ipv4Addr(libc::in_addr {
            s_addr: ((u32::from(a) << 24)
                | (u32::from(b) << 16)
                | (u32::from(c) << 8)
                | u32::from(d))
            .to_be(),
        })
    }
}

impl TcpListener {
    pub fn bind(addr: Ipv4Addr, port: u16) -> io::Result<TcpListener> {
        let backlog = 128;
        let sock = syscall!(socket(
            libc::PF_INET,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0
        ))?;
        let opt: i32 = 1;
        syscall!(setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &opt as *const _ as *const libc::c_void,
            std::mem::size_of_val(&opt) as u32
        ))?;

        let sin: libc::sockaddr_in = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: addr.0,
            ..unsafe { mem::zeroed() }
        };
        let addrp: *const libc::sockaddr = &sin as *const _ as *const _;
        let len = mem::size_of_val(&sin) as libc::socklen_t;

        syscall!(bind(sock, addrp, len))?;
        syscall!(listen(sock, backlog))?;

        println!("(TcpListner) listen: {}", sock);
        Ok(TcpListener(sock))
    }

    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }

    pub fn accept(&self) -> io::Result<TcpStream> {
        let mut sin_client: libc::sockaddr_in = unsafe { mem::zeroed() };
        let addrp: *mut libc::sockaddr = &mut sin_client as *mut _ as *mut _;
        let mut len: libc::socklen_t = unsafe { mem::zeroed() };
        let lenp: *mut _ = &mut len as *mut _;
        let sock_client = syscall!(accept(self.0, addrp, lenp))?;
        println!("(TcpStream)  accept: {}", sock_client);
        Ok(TcpStream(sock_client))
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        println!("(TcpListner) close : {}", self.0);
        syscall!(close(self.0)).ok();
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        println!("(TcpStream)  close : {}", self.0);
        syscall!(close(self.0)).ok();
    }
}

impl<'a> Iterator for Incoming<'a> {
    type Item = io::Result<TcpStream>;
    fn next(&mut self) -> Option<io::Result<TcpStream>> {
        Some(self.listener.accept())
    }
}

fn handle_client(stream: TcpStream) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    loop {
        let n = syscall!(read(
            stream.0,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len()
        ))?;
        if n == 0 {
            break;
        }
        // NOTE: write() possibly writes less than n bytes. So we should check the return value to
        // check how many bytes written but omit it here for simplicity.
        syscall!(write(
            stream.0,
            buf.as_ptr() as *const libc::c_void,
            n as usize
        ))?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = Ipv4Addr::new(127, 0, 0, 1);
    let port = 8080;

    let listner = TcpListener::bind(addr, port)?;
    for stream in listner.incoming() {
        let stream = stream?;
        handle_client(stream)?;
    }

    Ok(())
}
