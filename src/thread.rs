use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;

pub(crate) struct ThreadRunner {
    handle: Option<JoinHandle<()>>,
    stop_flag: StopFlag,
}

impl ThreadRunner {
    pub fn run<F>(mut f: F) -> ThreadRunner
    where
        F: FnMut(StopFlag) + 'static + Send,
    {
        let stop_flag = StopFlag(Arc::new(AtomicBool::new(false)));
        let thread_stop_flag = stop_flag.clone();
        let handle = thread::spawn(move || f(thread_stop_flag));
        ThreadRunner {
            handle: Some(handle),
            stop_flag,
        }
    }
}

impl Drop for ThreadRunner {
    fn drop(&mut self) {
        self.stop_flag.set();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
