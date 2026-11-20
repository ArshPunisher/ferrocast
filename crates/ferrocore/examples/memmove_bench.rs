//! Non-VideoToolbox micro-benchmark: what copying one 1440p NV12 frame (5.5 MB) into a fresh
//! destination costs when the source is warm in this core's cache (the old single-thread engine
//! encoding each output in turn) versus written by another core (an output on its own thread).
//! `cargo run --release -p ferrocore --example memmove_bench`
use std::sync::{Arc, Barrier};
use std::time::Instant;

const FRAME: usize = 2560 * 1440 * 3 / 2;
const N: usize = 600;

fn fill(buf: &mut [u8], seed: u8) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(seed);
    }
}

fn main() {
    // A: one thread makes the frame, then copies it twice in a row (two outputs, one thread).
    let mut src = vec![0u8; FRAME];
    let mut d1 = vec![0u8; FRAME];
    let mut d2 = vec![0u8; FRAME];
    let mut t = 0.0;
    for i in 0..N {
        fill(&mut src, i as u8);
        let t0 = Instant::now();
        d1.copy_from_slice(std::hint::black_box(&src));
        d2.copy_from_slice(std::hint::black_box(&src));
        std::hint::black_box((&d1, &d2));
        t += t0.elapsed().as_secs_f64();
    }
    println!("A same thread wrote it, 2 copies in turn: {:.2} ms per frame (both)", t / N as f64 * 1e3);

    // B: a producer thread writes the frame; two worker threads each copy it (the D1 layout).
    let src = Arc::new(std::sync::Mutex::new(Arc::new(vec![0u8; FRAME])));
    let go = Arc::new(Barrier::new(3));
    let done = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let (src, go, done) = (src.clone(), go.clone(), done.clone());
            std::thread::spawn(move || {
                let mut dst = vec![0u8; FRAME];
                let mut t = 0.0;
                for _ in 0..N {
                    go.wait();
                    let f = src.lock().unwrap().clone();
                    let t0 = Instant::now();
                    dst.copy_from_slice(std::hint::black_box(&f));
                    std::hint::black_box(&dst);
                    t += t0.elapsed().as_secs_f64();
                    done.wait();
                }
                t / N as f64
            })
        })
        .collect();
    for i in 0..N {
        let mut f = vec![0u8; FRAME];
        fill(&mut f, i as u8);
        *src.lock().unwrap() = Arc::new(f);
        go.wait();
        done.wait();
    }
    let per: Vec<f64> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    println!(
        "B another core wrote it, 2 workers copy at once: {:.2} ms and {:.2} ms per frame each",
        per[0] * 1e3,
        per[1] * 1e3
    );

    // C: the same two copies, source cold (written by another thread), on one thread in turn.
    let src = Arc::new(std::sync::Mutex::new(vec![0u8; FRAME]));
    let (a, b) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let producer = {
        let (src, a, b) = (src.clone(), a.clone(), b.clone());
        std::thread::spawn(move || {
            for i in 0..N {
                fill(&mut src.lock().unwrap(), i as u8);
                a.wait();
                b.wait();
            }
        })
    };
    let (mut d1, mut d2) = (vec![0u8; FRAME], vec![0u8; FRAME]);
    let mut t = 0.0;
    for _ in 0..N {
        a.wait();
        let s = src.lock().unwrap();
        let t0 = Instant::now();
        d1.copy_from_slice(std::hint::black_box(&s));
        d2.copy_from_slice(std::hint::black_box(&s));
        std::hint::black_box((&d1, &d2));
        t += t0.elapsed().as_secs_f64();
        drop(s);
        b.wait();
    }
    producer.join().unwrap();
    println!("C another core wrote it, 2 copies in turn on one thread: {:.2} ms per frame (both)", t / N as f64 * 1e3);
}
