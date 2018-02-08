// Indebted to "The Art of Multiprocessor Programming"

use std::sync::{Condvar, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::mem;

unsafe impl<T: ::std::fmt::Debug> Send for Queue<T> {}
unsafe impl<T: ::std::fmt::Debug> Sync for Queue<T> {}

struct InnerQueue<T>
where
    T: ::std::fmt::Debug,
{
    capacity: usize,
    data: *mut Option<T>,
    size: AtomicUsize,
    enq_lock: Mutex<isize>,
    deq_lock: Mutex<isize>,
    not_empty: Condvar,
}

#[derive(Debug)]
pub enum Error {
    WouldBlock,
}

impl<T> InnerQueue<T>
where
    T: ::std::fmt::Debug,
{
    pub fn new() -> InnerQueue<T> {
        InnerQueue::with_capacity(1024)
    }

    pub fn with_capacity(capacity: usize) -> InnerQueue<T> {
        let mut data: Vec<Option<T>> = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            data.push(None);
        }
        InnerQueue {
            capacity: capacity,
            data: (&mut data).as_mut_ptr(),
            size: AtomicUsize::new(0),
            enq_lock: Mutex::new(0),
            deq_lock: Mutex::new(0),
            not_empty: Condvar::new(),
        }
    }

    pub fn enq(&mut self, elem: T) -> Result<(), Error> {
        let mut must_wake_dequeuers = false;
        let mut guard = self.enq_lock.lock().expect("enq guard poisoned");
        let ptr: &mut Option<T> = unsafe {
            self.data
                .offset(*guard)
                .as_mut()
                .expect("enq pointer is null")
        };
        if ptr.is_some() {
            return Err(Error::WouldBlock);
        } else {
            assert!(mem::replace(ptr, Some(elem)).is_none());
            *guard += 1;
            *guard %= self.capacity as isize;
            if self.size.fetch_add(1, Ordering::Relaxed) == 0 {
                must_wake_dequeuers = true;
            };
        }
        drop(guard);
        if must_wake_dequeuers {
            let guard = self.deq_lock.lock().expect("deq guard poisoned");
            self.not_empty.notify_all();
            drop(guard);
        }
        return Ok(());
    }

    pub fn deq(&mut self) -> T {
        let mut guard = self.deq_lock.lock().expect("deq guard poisoned");
        while self.size.load(Ordering::Relaxed) == 0 {
            guard = self.not_empty.wait(guard).unwrap();
        }
        let ptr: &mut Option<T> =
            unsafe { self.data.offset(*guard).as_mut().expect("deq pointer null") };
        match mem::replace(ptr, None) {
            Some(elem) => {
                *guard += 1;
                *guard %= self.capacity as isize;
                self.size.fetch_sub(1, Ordering::Relaxed);
                return elem;
            }
            None => unreachable!(),
        }
    }
}

struct Queue<T>
where
    T: ::std::fmt::Debug,
{
    // we need to use unsafe cell so that innerqueue has a fixed place in memory
    inner: *mut InnerQueue<T>,
}

impl<T> Clone for Queue<T>
where
    T: ::std::fmt::Debug,
{
    fn clone(&self) -> Queue<T> {
        Queue { inner: self.inner }
    }
}

#[allow(dead_code)]
impl<T> Queue<T>
where
    T: ::std::fmt::Debug,
{
    pub fn with_capacity(capacity: usize) -> Queue<T> {
        let inner = Box::into_raw(Box::new(InnerQueue::with_capacity(capacity)));
        Queue { inner: inner }
    }

    pub fn new() -> Queue<T> {
        Queue::with_capacity(1024)
    }

    pub fn enq(&mut self, elem: T) -> Result<(), Error> {
        unsafe { (*self.inner).enq(elem) }
    }

    pub fn deq(&mut self) -> T {
        unsafe { (*self.inner).deq() }
    }
}

#[cfg(test)]
mod test {
    extern crate quickcheck;

    use self::quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};
    use std::thread;
    use super::*;

    #[derive(Clone, Debug)]
    enum Action {
        Enq(u64),
        Deq,
    }

    impl Arbitrary for Action {
        fn arbitrary<G>(g: &mut G) -> Action
        where
            G: Gen,
        {
            let i: usize = g.gen_range(0, 100);
            match i {
                0...50 => Action::Enq(g.gen::<u64>()),
                _ => Action::Deq,
            }
        }
    }

    #[test]
    fn sequential_model_check() {
        fn inner(actions: Vec<Action>) -> TestResult {
            use std::collections::VecDeque;

            let mut model: VecDeque<u64> = VecDeque::new();
            let mut sut: Queue<u64> = Queue::new();

            for action in actions {
                match action {
                    Action::Enq(v) => {
                        model.push_back(v);
                        assert!(sut.enq(v).is_ok());
                    }
                    Action::Deq => match model.pop_front() {
                        Some(v) => {
                            assert_eq!(v, sut.deq());
                        }
                        None => continue,
                    },
                }
            }
            TestResult::passed()
        }
        QuickCheck::new().quickcheck(inner as fn(Vec<Action>) -> TestResult);
    }

    #[test]
    fn model_check() {
        fn inner(total_senders: usize, capacity: usize, vals: Vec<u64>) -> TestResult {
            println!(
                "MODEL CHECK\n    SENDERS: {}\n    CAPACITY: {}\n    VALUES: {:?}",
                total_senders, capacity, vals
            );
            if total_senders == 0 || capacity == 0 {
                return TestResult::discard();
            }

            let mut sut: Queue<u64> = Queue::with_capacity(capacity);

            let mut snd_jh = Vec::new();
            let snd_vals = vals.clone();
            for chunk in snd_vals.chunks(total_senders) {
                let mut snd_q = sut.clone();
                let chunk: Vec<u64> = chunk.to_vec();
                snd_jh.push(thread::spawn(move || {
                    let mut queued: Vec<u64> = Vec::new();
                    for ev in chunk {
                        if snd_q.enq(ev).is_ok() {
                            queued.push(ev);
                        }
                    }
                    queued
                }))
            }

            let expected_total_vals = vals.len();
            let rcv_jh = thread::spawn(move || {
                let mut collected: Vec<u64> = Vec::new();
                while collected.len() < expected_total_vals {
                    let v = sut.deq();
                    collected.push(v);
                }
                collected
            });

            let mut snd_vals: Vec<u64> = Vec::new();
            for jh in snd_jh {
                snd_vals.append(&mut jh.join().expect("snd join failed"));
            }
            let mut rcv_vals: Vec<u64> = rcv_jh.join().expect("rcv join failed");

            rcv_vals.sort();
            snd_vals.sort();

            assert_eq!(rcv_vals, snd_vals);
            TestResult::passed()
        }
        QuickCheck::new().quickcheck(inner as fn(usize, usize, Vec<u64>) -> TestResult);
    }
}
