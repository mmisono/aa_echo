// Non-blocking echo server using epoll.

use std::collections::HashMap;
use std::io;
use std::mem;
use std::os::unix::io::RawFd;

#[macro_use]
mod util;

struct Ipv4Addr(libc::in_addr);
struct TcpListener(RawFd);
struct TcpStream(RawFd);

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

impl TcpStream {
    pub fn setnonblocking(&self) -> io::Result<()> {
        let flag = syscall!(fcntl(self.0, libc::F_GETFL, 0))?;
        syscall!(fcntl(self.0, libc::F_SETFL, flag | libc::O_NONBLOCK))?;
        Ok(())
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

struct Epoll {
    fd: RawFd,
    events: Vec<libc::epoll_event>,
}
enum EpollEventType {
    // Only event types used in this example
    In,
    Out,
}

impl Epoll {
    pub fn new(max_event: usize) -> io::Result<Self> {
        let fd = syscall!(epoll_create1(libc::EPOLL_CLOEXEC))?;
        let event: libc::epoll_event = unsafe { mem::zeroed() };
        let events = vec![event; max_event];
        Ok(Epoll { fd, events })
    }

    fn run_ctl(&self, epoll_ctl: libc::c_int, fd: RawFd, op: EpollEventType) -> io::Result<()> {
        let mut event: libc::epoll_event = unsafe { mem::zeroed() };
        event.u64 = fd as u64;
        event.events = match op {
            EpollEventType::In => libc::EPOLLIN as u32,
            EpollEventType::Out => libc::EPOLLOUT as u32,
        };

        let eventp: *mut _ = &mut event as *mut _;
        syscall!(epoll_ctl(self.fd, epoll_ctl, fd, eventp))?;

        Ok(())
    }

    pub fn add_event(&self, fd: RawFd, op: EpollEventType) -> io::Result<()> {
        self.run_ctl(libc::EPOLL_CTL_ADD, fd, op)
    }

    pub fn mod_event(&self, fd: RawFd, op: EpollEventType) -> io::Result<()> {
        self.run_ctl(libc::EPOLL_CTL_MOD, fd, op)
    }

    pub fn del_event(&self, fd: RawFd) -> io::Result<()> {
        syscall!(epoll_ctl(
            self.fd,
            libc::EPOLL_CTL_DEL,
            fd,
            std::ptr::null_mut() as *mut libc::epoll_event
        ))?;

        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<usize> {
        let nfd = syscall!(epoll_wait(
            self.fd,
            self.events.as_mut_ptr(),
            self.events.len() as i32,
            -1 // no timeout
        ))?;

        Ok(nfd as usize)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        syscall!(close(self.fd)).ok();
    }
}

#[allow(dead_code)]
struct ClientState {
    stream: TcpStream,
    buf: [u8; 1024],
    buf_cursor: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = Ipv4Addr::new(127, 0, 0, 1);
    let port = 8080;

    let mut epoll = Epoll::new(32)?;
    let listner = TcpListener::bind(addr, port)?;
    let mut clients: HashMap<RawFd, ClientState> = HashMap::new();

    epoll.add_event(listner.0, EpollEventType::In)?;

    loop {
        let nfd = epoll.wait()?;

        for i in 0..nfd {
            let fd = epoll.events[i].u64 as RawFd;
            if fd == listner.0 {
                /* Accept a new connection */
                let client = listner.accept()?;
                client.setnonblocking()?;
                epoll.add_event(client.0, EpollEventType::In)?;
                clients.insert(
                    client.0,
                    ClientState {
                        stream: client,
                        buf: [0u8; 1024],
                        buf_cursor: 0,
                    },
                );
            } else if clients.contains_key(&fd) {
                /* Handle a client */
                let events = epoll.events[i].events as i32;
                let client_state = clients.get_mut(&fd).unwrap();
                if (events & libc::EPOLLIN) > 0 {
                    /* read */
                    let n = syscall!(read(
                        fd,
                        client_state.buf.as_mut_ptr() as *mut libc::c_void,
                        client_state.buf.len()
                    ))?;
                    client_state.buf_cursor = n as usize;
                    if n == 0 {
                        epoll.del_event(fd)?;
                        clients.remove(&fd);
                        break;
                    }
                    epoll.mod_event(fd, EpollEventType::Out)?;
                } else if (events & libc::EPOLLOUT) > 0 {
                    /* write */
                    syscall!(write(
                        fd,
                        client_state.buf.as_ptr() as *const libc::c_void,
                        client_state.buf_cursor
                    ))?;
                    epoll.mod_event(fd, EpollEventType::In)?;
                } else {
                    unreachable!();
                }
            } else {
                unreachable!();
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}
