// https://github.com/tokio-rs/mio/blob/803573fa28244a513ea10d97fc7df725dfa9ce9b/src/sys/unix/mod.rs#L4
// invoke a system call and convert the result into io::Result
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}
