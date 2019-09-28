// async/await non-blocking echo server

use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::io;
use std::io::Write;
use std::mem;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::future::{BoxFuture, FutureExt};
use futures::task::{waker_ref, ArcWake};

use lazy_static::lazy_static;
use log::info;

#[macro_use]
mod util;

struct Ipv4Addr(libc::in_addr);
struct TcpListener(RawFd);
struct TcpStream(RawFd);
struct Incoming<'a>(&'a TcpListener);

struct Reactor {
    epoll: Epoll,
    wakers: Mutex<HashMap<RawFd, Waker>>,
}
struct AcceptFuture<'a>(&'a TcpListener);
struct ReadFuture<'a>(&'a TcpStream, &'a mut [u8]);
struct WriteFuture<'a>(&'a TcpStream, &'a [u8]);

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
    // NOTE: bind() may be block. So this should be an async function in reality.
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

        info!("(TcpListner) listen: {}", sock);
        let listner = TcpListener(sock);
        listner.setnonblocking()?;
        Ok(listner)
    }

    pub fn accept(&self) -> io::Result<TcpStream> {
        let mut sin_client: libc::sockaddr_in = unsafe { mem::zeroed() };
        let addrp: *mut libc::sockaddr = &mut sin_client as *mut _ as *mut _;
        let mut len: libc::socklen_t = unsafe { mem::zeroed() };
        let lenp: *mut _ = &mut len as *mut _;
        let sock_client = syscall!(accept(self.0, addrp, lenp))?;
        info!("(TcpStream)  accept: {}", sock_client);
        Ok(TcpStream(sock_client))
    }

    pub fn incoming(&self) -> Incoming<'_> {
        Incoming(self)
    }

    pub fn setnonblocking(&self) -> io::Result<()> {
        let flag = syscall!(fcntl(self.0, libc::F_GETFL, 0))?;
        syscall!(fcntl(self.0, libc::F_SETFL, flag | libc::O_NONBLOCK))?;
        Ok(())
    }
}

impl<'a> Incoming<'a> {
    pub fn next(&self) -> AcceptFuture<'a> {
        AcceptFuture(self.0)
    }
}

impl<'a> Future for AcceptFuture<'a> {
    type Output = Option<io::Result<TcpStream>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.accept() {
            Ok(stream) => {
                stream.setnonblocking()?;
                Poll::Ready(Some(Ok(stream)))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                REACTOR.add_event((self.0).0, EpollEventType::In, cx.waker().clone())?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Some(Err(e))),
        }
    }
}

impl TcpStream {
    pub fn setnonblocking(&self) -> io::Result<()> {
        let flag = syscall!(fcntl(self.0, libc::F_GETFL, 0))?;
        syscall!(fcntl(self.0, libc::F_SETFL, flag | libc::O_NONBLOCK))?;
        Ok(())
    }

    pub fn read<'a>(&'a self, buf: &'a mut [u8]) -> ReadFuture<'a> {
        ReadFuture(self, buf)
    }

    pub fn write<'a>(&'a self, buf: &'a [u8]) -> WriteFuture<'a> {
        WriteFuture(self, buf)
    }
}

impl<'a> Future for ReadFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = syscall!(read(
            (self.0).0,
            self.1.as_mut_ptr() as *mut libc::c_void,
            self.1.len()
        ));
        match res {
            Ok(n) => Poll::Ready(Ok(n as usize)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                REACTOR.add_event((self.0).0, EpollEventType::In, cx.waker().clone())?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl<'a> Future for WriteFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = syscall!(write(
            (self.0).0,
            self.1.as_ptr() as *mut libc::c_void,
            self.1.len()
        ));
        match res {
            Ok(n) => Poll::Ready(Ok(n as usize)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                REACTOR.add_event((self.0).0, EpollEventType::Out, cx.waker().clone())?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        info!("(TcpListner) close : {}", self.0);
        syscall!(close(self.0)).ok();
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        info!("(TcpStream)  close : {}", self.0);
        syscall!(close(self.0)).ok();
    }
}

lazy_static! {
    static ref REACTOR: Reactor = {
        // Start reactor main loop
        std::thread::spawn(move || {
            reactor_main_loop()
        });

        Reactor {
            epoll: Epoll::new().expect("Failed to create epoll"),
            wakers: Mutex::new(HashMap::new())
        }
    };
}

impl Reactor {
    pub fn add_event(&self, fd: RawFd, op: EpollEventType, waker: Waker) -> io::Result<()> {
        info!("(Reactor) add event: {}", fd);
        self.epoll.add_event(fd, op)?;
        self.wakers.lock().unwrap().insert(fd, waker);
        Ok(())
    }
}

fn reactor_main_loop() -> io::Result<()> {
    info!("Start reactor main loop");
    let max_event = 32;
    let event: libc::epoll_event = unsafe { mem::zeroed() };
    let mut events = vec![event; max_event];
    let reactor = &REACTOR;

    loop {
        let nfd = reactor.epoll.wait(&mut events)?;
        info!("(Reacotr) wake up. nfd = {}", nfd);

        #[allow(clippy::needless_range_loop)]
        for i in 0..nfd {
            let fd = events[i].u64 as RawFd;
            let waker = reactor
                .wakers
                .lock()
                .unwrap()
                .remove(&fd)
                .unwrap_or_else(|| panic!("not found fd {}", fd));
            info!("(Reacotr) delete event: {}", fd);
            reactor.epoll.del_event(fd)?;
            waker.wake();
        }
    }
}

struct Epoll {
    fd: RawFd,
}

enum EpollEventType {
    // Only event types used in this example
    In,
    Out,
}

impl Epoll {
    pub fn new() -> io::Result<Self> {
        let fd = syscall!(epoll_create1(libc::EPOLL_CLOEXEC))?;
        Ok(Epoll { fd })
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

    #[allow(dead_code)]
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

    pub fn wait(&self, events: &mut [libc::epoll_event]) -> io::Result<usize> {
        let nfd = syscall!(epoll_wait(
            self.fd,
            events.as_mut_ptr(),
            events.len() as i32,
            -1
        ))?;

        Ok(nfd as usize)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        syscall!(close(self.fd)).ok();
    }
}

struct Task {
    future: Mutex<Option<BoxFuture<'static, io::Result<()>>>>,
    task_sender: SyncSender<Arc<Task>>,
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let cloned = arc_self.clone();
        arc_self.task_sender.send(cloned).expect("failed to send");
    }
}

struct Executor {
    ready_queue: Receiver<Arc<Task>>,
}

impl Executor {
    fn run(&self) {
        while let Ok(task) = self.ready_queue.recv() {
            let mut future_slot = task.future.lock().unwrap();
            if let Some(mut future) = future_slot.take() {
                let waker = waker_ref(&task);
                let mut context = Context::from_waker(&*waker);
                if let Poll::Pending = future.as_mut().poll(&mut context) {
                    *future_slot = Some(future);
                }
            }
        }
    }
}

#[derive(Clone)]
struct Spawner {
    task_sender: SyncSender<Arc<Task>>,
}

impl Spawner {
    fn spawn(&self, fut: impl Future<Output = io::Result<()>> + 'static + Send) {
        let fut = fut.boxed();
        let task = Arc::new(Task {
            future: Mutex::new(Some(fut)),
            task_sender: self.task_sender.clone(),
        });
        self.task_sender.send(task).expect("failed to send");
    }
}

fn new_executor_and_spawner() -> (Executor, Spawner) {
    let (task_sender, ready_queue) = sync_channel(10000);
    (Executor { ready_queue }, Spawner { task_sender })
}

fn init_log() {
    // format = [file:line] msg
    env::set_var("RUST_LOG", "info");
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            writeln!(
                buf,
                "[{}:{:>3}] {}",
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.args(),
            )
        })
        .init();
}

async fn handle_client(stream: TcpStream) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    info!("(handle client) {}", stream.0);
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write(&buf[..n]).await?;
    }
    Ok(())
}

fn main() {
    init_log();

    let (executor, spawner) = new_executor_and_spawner();
    let spawner_clone = spawner.clone();

    let mainloop = async move {
        let addr = Ipv4Addr::new(127, 0, 0, 1);
        let port = 8080;
        let listner = TcpListener::bind(addr, port)?;

        let incoming = listner.incoming();

        while let Some(stream) = incoming.next().await {
            let stream = stream?;
            spawner.spawn(handle_client(stream));
        }

        Ok(())
    };

    spawner_clone.spawn(mainloop);
    drop(spawner_clone);
    executor.run();
}
