//! A thread that periodically sends a newly calibrated `Clocksource` to subscribers.
//! The use case is providing a hot path new clocks it can swap in without
//! ever needing to pause for about 1 second to calibrate.
//! 

use std::sync::mpsc::{Sender, Receiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{Instant, Duration};
use triple_buffer::{TripleBuffer, Input, Output};
use {Clock, Clocksource};

/// Holds and provides a subscription interface to a thread that periodically
/// sends a newly calibrated `Clocksource` to subscribers via a `mpsc::channel`.
/// 
/// # Examples
/// 
/// ```
/// extern crate clocksource;
/// 
/// use std::thread::{self, JoinHandle};
/// use std::sync::mpsc::{Sender, Receiver, channel};
/// use std::time::{Instant, Duration};
/// use clocksource::Clocksource;
/// use clocksource::calibrator::Calibrator;
/// 
/// fn event_loop(clocks: Receiver<Clocksource>) -> JoinHandle<usize> {
///     thread::spawn(move || {
///         let mut clock = clocks.recv().unwrap();
///         let mut clocks_rcvd = 1;
///         let start = clock.time();
///         loop {
///             // guys, I'm super busy... no time to calibrate.
///             
///             let loop_time = clock.time(); // fast!
///             
///             if let Ok(c) = clocks.try_recv() { 
///                 clock = c;
///                 clocks_rcvd += 1;
///             }
///             
///             // clock is not monotonic, hence the use of `saturating_sub`
///             if loop_time.saturating_sub(start) > 2_500_000_000 { break }
///         }
///         
///         clocks_rcvd
///     })
/// }
/// 
/// fn main() {
///     let start = Instant::now();
///     let calibrator = Calibrator::with_freq(Duration::from_secs(2));
///     let hot_path = event_loop(calibrator.subscribe());
///     let n_clocks = hot_path.join().unwrap();
///     let took = Instant::now() - start;
///     println!("{:?}", took);
///     assert!(took > Duration::from_secs(3));
///     assert_eq!(n_clocks, 2);
/// }
/// ```
/// 
pub struct Calibrator {
    tx: Sender<Option<Sender<Clocksource>>>,
    thread: Option<JoinHandle<()>>,
}

/// Holds and provides a subscription interface to a thread that periodically
/// writes a newly calibrated `Clocksource` to shared buffer.
/// 
/// # Examples
/// 
/// ```
/// extern crate clocksource;
/// extern crate triple_buffer;
/// 
/// use std::thread::{self, JoinHandle};
/// use std::time::Duration;
/// use triple_buffer::Output;
/// use clocksource::Clocksource;
/// use clocksource::calibrator::TripleBufferCalibrator;
/// 
/// fn event_loop(mut clock: Output<Clocksource>) -> JoinHandle<()> {
///     thread::spawn(move || {
///         let start = clock.read().time();
///         loop {
///             // guys, I'm super busy... no time to calibrate.
///             
///             let loop_time = clock.read().time();
///             
///             // not monotonic, hence the use of `saturating_sub`
///             if loop_time.saturating_sub(start) > 1_000_000_000 { break }
///         }
///     })
/// }
/// 
/// fn main() {
///     let calibrator = TripleBufferCalibrator::with_freq(Duration::from_secs(2));
///     let hot_path = event_loop(calibrator.subscribe());
///     let _ = hot_path.join();
/// }
/// ```
/// 
pub struct TripleBufferCalibrator {
    tx: Sender<Option<Input<Clocksource>>>,
    thread: Option<JoinHandle<()>>,
    clock: Clocksource,
}

macro_rules! duplicate_functionality {
    ($t:ident) => {
        impl $t {
            /// Instantiates a `Clocksource` by passing `reference` and `source` to
            /// `Clocksource::configured`, then calibrates and sends copies to all
            /// subscribers approximately every `freq`.
            /// 
            pub fn new(reference: Clock, source: Clock, freq: Duration) -> Self {
                let clock = Clocksource::configured(reference, source);
                Self::with_clock(clock, freq)
            }

            /// Like `new` but supplies the `Default` `Clocksource` rather than
            /// requiring `reference` and `source` to be specified. Clock calibration
            /// will happen approximately every `freq`.
            /// 
            pub fn with_freq(freq: Duration) -> Self {
                let clock = Clocksource::default();
                Self::with_clock(clock, freq)
            }
        }

        impl Default for $t {
            fn default() -> Self {
                let freq = Duration::from_secs(30);

                if cfg!(feature = "rdtsc") {
                    Self::new(Clock::Realtime, Clock::Counter, freq)
                } else {
                    Self::new(Clock::Realtime, Clock::Monotonic, freq)
                }
            }
        }

        impl Drop for $t {
            fn drop(&mut self) {
                let _ = self.tx.send(None);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
    }
}

duplicate_functionality!(Calibrator);
duplicate_functionality!(TripleBufferCalibrator);

impl Calibrator {
    fn with_clock(mut clock: Clocksource, freq: Duration) -> Self {
        let (tx, rx) = channel();
        let thread = Some(thread::spawn(move || {
            let mut subscribers: Vec<Sender<Clocksource>> = Vec::new();
            let mut last_sent = Instant::now();
            loop {
                let loop_time = Instant::now();
                let delta = loop_time - last_sent;
                let timeout = if freq > delta { freq - delta } else { Duration::new(0, 0) };
                match rx.recv_timeout(timeout) {
                    Ok(Some(s)) => {
                        subscribers.push(s);
                    }

                    // `None` used as terminate signal from `Drop`
                    Ok(None) => break,

                    _ => {}
                }

                if loop_time - last_sent > freq {
                    clock.calibrate();
                    for s in &subscribers {
                        let _ = s.send(clock.clone());
                    }
                    last_sent = loop_time;
                }
            }
        }));

        Self { tx, thread }
    }

    /// Creates a new channel for the callee to periodically receive newly
    /// calibrated clocks. The sender is sent on to the calibrator thread,
    /// which stores it.
    /// 
    pub fn subscribe(&self) -> Receiver<Clocksource> {
        let (tx, rx) = channel();
        self.tx.send(Some(tx)).unwrap();
        rx
    }
}

impl TripleBufferCalibrator {
    fn with_clock(mut clock: Clocksource, freq: Duration) -> Self {
        let (tx, rx) = channel();
        let clock_copy = clock.clone();
        let thread = Some(thread::spawn(move || {
            let mut subscribers: Vec<Input<Clocksource>> = Vec::new();
            let mut last_sent = Instant::now();
            loop {
                let loop_time = Instant::now();
                let delta = loop_time - last_sent;
                let timeout = if freq > delta { freq - delta } else { Duration::new(0, 0) };
                match rx.recv_timeout(timeout) {
                    Ok(Some(s)) => {
                        subscribers.push(s);
                    }

                    // `None` used as terminate signal from `Drop`
                    Ok(None) => break,

                    _ => {}
                }

                if loop_time - last_sent > freq {
                    clock.calibrate();
                    for s in subscribers.iter_mut() {
                        s.write(clock.clone());
                    }
                    last_sent = loop_time;
                }
            }
        }));

        Self { tx, thread, clock: clock_copy }
    }

    /// Creates a new channel for the callee to periodically receive newly
    /// calibrated clocks. The sender is sent on to the calibrator thread,
    /// which stores it.
    /// 
    pub fn subscribe(&self) -> Output<Clocksource> {
        let mut clock = self.clock.clone();
        clock.calibrate();
        let buf = TripleBuffer::new(clock);
        let (wtr, rdr) = buf.split();
        self.tx.send(Some(wtr)).unwrap();
        rdr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test::Bencher;

    #[cfg(feature = "rdtsc")]
    #[ignore]
    #[bench]
    fn calibrates_a_realtime_counter_clocksource(b: &mut Bencher) {
        let mut clock = Clocksource::configured(Clock::Realtime, Clock::Counter);
        b.iter(|| {
            clock.calibrate()
        });
    }

    #[bench]
    fn cloning_a_clocksource(b: &mut Bencher) {
        let clock = Clocksource::configured(Clock::Realtime, Clock::Counter);
        b.iter(|| clock.clone());
    }

    #[cfg(feature = "rdtsc")]
    #[bench]
    fn mpsc_calibrator_hot_path_loop_overhead(b: &mut Bencher) {
        let calibrator = Calibrator::new(Clock::Realtime, Clock::Counter, Duration::from_secs(2));
        let clocks = calibrator.subscribe();
        let mut clock = clocks.recv().unwrap();
        let mut i = 0;
        b.iter(|| {
            i = match i {
                j @ 0 ... 10_000 => j + 1,

                _ => {
                    if let Ok(c) = clocks.try_recv() { 
                        clock = c;
                    }
                    0
                }
            };
            clock.time()
        });
    }

    #[cfg(feature = "rdtsc")]
    #[bench]
    fn triple_buffer_calibrator_hot_path_loop_overhead(b: &mut Bencher) {
        let calibrator = TripleBufferCalibrator::new(Clock::Realtime, Clock::Counter, Duration::from_secs(30));
        let mut clock = calibrator.subscribe();
        b.iter(|| {
            clock.read().time()
        });
    }
}
