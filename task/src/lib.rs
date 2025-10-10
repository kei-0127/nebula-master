use std::thread;

use anyhow::Result;
use crossbeam::channel::{self, Receiver, Sender};
use lazy_static::lazy_static;
use tokio::{self, sync::oneshot};

type Job = Box<dyn FnOnce() + Send + 'static>;

lazy_static! {
    pub static ref THREAD_POOL: ThreadPool = ThreadPool::new("task");
    pub static ref PRIORITY_THREAD_POOL: ThreadPool =
        ThreadPool::new("priority_task");
}

pub struct ThreadPool {
    sender: Sender<Job>,
    name: &'static str,
}

pub struct Worker {
    receiver: Receiver<Job>,
    name: &'static str,
}

impl ThreadPool {
    pub fn new(name: &'static str) -> Self {
        let (sender, receiver) = channel::unbounded();
        let n = num_cpus::get();
        for _ in 0..n {
            Worker::new(receiver.clone(), name);
        }
        Self { sender, name }
    }

    pub fn spawn<F>(&self, func: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let _ = self.sender.send(Box::new(func));
    }
}

impl Worker {
    pub fn new(receiver: Receiver<Job>, name: &'static str) {
        let _ = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let worker = Worker { receiver, name };
                worker.run();
            });
    }

    pub fn run(&self) {
        loop {
            let job = self.receiver.recv().unwrap();
            job();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        println!("thread pool worker dropped");
        let receiver = self.receiver.clone();
        Worker::new(receiver, self.name);
    }
}

pub async fn spawn_task<F, R>(func: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    THREAD_POOL.spawn(move || {
        let result = func();
        let _ = sender.send(result);
    });
    let result = receiver.await?;
    Ok(result)
}

pub async fn spawn_priority_task<F, R>(func: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    PRIORITY_THREAD_POOL.spawn(move || {
        let result = func();
        let _ = sender.send(result);
    });
    let result = receiver.await?;
    Ok(result)
}

pub async fn spawn_rayon_task<F, R>(func: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    rayon::spawn_fifo(move || {
        let result = func();
        let _ = sender.send(result);
    });
    let result = receiver.await?;
    Ok(result)
}
