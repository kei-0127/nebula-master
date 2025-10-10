use core::future::Future;
use core::pin::Pin;
use std::time;
use std::{
    os::unix::io::{AsRawFd, RawFd},
    task::Poll,
};
use std::{task::Context, time::Duration};
use tokio::io::unix::AsyncFd;

use libc;

pub struct Timer {
    fd: AsyncFd<RawFd>,
    time: Duration,
    timeout: Duration,
}

impl Drop for Timer {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd.as_raw_fd());
        }
    }
}

impl Timer {
    pub fn new(timeout: time::Duration) -> std::io::Result<Self> {
        let fd = unsafe {
            libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK)
        };
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };

        let time = Duration::from_secs(time.tv_sec as u64)
            + Duration::from_nanos(time.tv_nsec as u64);

        let mut timer = Self {
            fd: AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)?,
            time,
            timeout,
        };

        timer.reset();
        Ok(timer)
    }

    fn reset(&mut self) {
        self.time += self.timeout;

        let timer = libc::itimerspec {
            it_value: libc::timespec {
                tv_sec: self.time.as_secs() as libc::time_t,
                tv_nsec: libc::suseconds_t::from(self.time.subsec_nanos()),
            },
            it_interval: unsafe { core::mem::MaybeUninit::zeroed().assume_init() },
        };
        unsafe {
            libc::timerfd_settime(
                self.fd.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &timer,
                core::ptr::null_mut(),
            )
        };
    }

    pub async fn tick(&mut self) {
        poll_fn(|cx| -> Poll<()> {
            match self.fd.poll_read_ready(cx) {
                Poll::Ready(ready) => {
                    ready.unwrap().clear_ready();
                    match self.read() {
                        0 => Poll::Pending,
                        _ => {
                            self.reset();
                            Poll::Ready(())
                        }
                    }
                }
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
    }

    fn read(&self) -> usize {
        let mut read_num = 0u64;
        match unsafe {
            libc::read(self.fd.as_raw_fd(), &mut read_num as *mut u64 as *mut _, 8)
        } {
            -1 => {
                let error = std::io::Error::last_os_error();
                match error.kind() {
                    std::io::ErrorKind::WouldBlock => 0,
                    _ => panic!("Unexpected read error: {}", error),
                }
            }
            _ => read_num as usize,
        }
    }
}

struct PollFn<F> {
    f: F,
}

impl<F> Unpin for PollFn<F> {}

impl<T, F> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        (&mut self.f)(cx)
    }
}

fn poll_fn<T, F>(f: F) -> PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    PollFn { f }
}
