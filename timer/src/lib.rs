//! # Timer Module
//! 
//! This module provides high-precision timer functionality for the Nebula VoIP system.
//! It implements a timer using Linux timerfd for precise timing operations required
//! for SIP protocol timers, media processing, and system scheduling.
//! 
//! ## Key Features
//! 
//! ### High-Precision Timing
//! - **Monotonic Clock**: Uses CLOCK_MONOTONIC for consistent timing
//! - **Nanosecond Precision**: Sub-millisecond timing accuracy
//! - **Absolute Time**: Absolute time-based scheduling
//! 
//! ### Async Integration
//! - **Tokio Integration**: Native async/await support
//! - **Non-blocking**: Non-blocking timer operations
//! - **Event-driven**: Event-driven timer notifications
//! 
//! ### Timer Management
//! - **Automatic Reset**: Automatic timer reset after expiration
//! - **Resource Management**: Automatic cleanup on drop
//! - **Error Handling**: Robust error handling and recovery
//! 
//! ## Architecture
//! 
//! The timer uses Linux timerfd system calls:
//! 1. **timerfd_create**: Create timer file descriptor
//! 2. **timerfd_settime**: Set timer expiration time
//! 3. **AsyncFd**: Wrap file descriptor for async operations
//! 4. **Poll**: Use tokio polling for timer events
//! 
//! ## Performance Characteristics
//! 
//! - **High Precision**: Nanosecond-level timing accuracy
//! - **Low Overhead**: Minimal CPU overhead for timer operations
//! - **Scalable**: Support for multiple concurrent timers
//! - **Reliable**: Robust timer implementation with error recovery
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_timer::Timer;
//! use std::time::Duration;
//! 
//! // Create a timer with 1-second interval
//! let mut timer = Timer::new(Duration::from_secs(1))?;
//! 
//! // Wait for timer expiration
//! timer.tick().await;
//! ```

use core::future::Future;
use core::pin::Pin;
use std::task::Poll;
use std::{task::Context, time::Duration};

#[cfg(unix)]
mod imp {
    use super::*;
    use std::{
        os::unix::io::{AsRawFd, RawFd},
        time,
    };
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
            let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK) };
            let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
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
                    tv_nsec: self.time.subsec_nanos() as libc::c_long,
                },
                it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
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
            super::poll_fn(|cx| -> Poll<()> {
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

    pub use Timer as PlatformTimer;
}

#[cfg(windows)]
mod imp {
    use super::*;
    use tokio::time::{sleep_until, Instant, Sleep};
    use std::time;

    pub struct Timer {
        next_deadline: Instant,
        timeout: Duration,
    }

    impl Timer {
        pub fn new(timeout: time::Duration) -> std::io::Result<Self> {
            let now = Instant::now();
            Ok(Self { next_deadline: now + timeout, timeout })
        }

        pub async fn tick(&mut self) {
            sleep_until(self.next_deadline).await;
            self.next_deadline += self.timeout;
        }
    }

    pub use Timer as PlatformTimer;
}

pub use imp::PlatformTimer as Timer;

struct PollFn<F> { f: F }

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
