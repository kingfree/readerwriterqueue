use rand::prelude::*;
use readerwriterqueue::ReaderWriterQueue;
use std::thread;

#[test]
fn test() {
    let mut rng = rand::thread_rng();
    let mut i = 0;
    loop {
        i += 1;
        println!("Test {i}");

        let n: usize = rng.gen_range(1..32);
        let mut q = ReaderWriterQueue::<usize>::with_size(n);

        let writer = thread::Builder::new()
            .spawn(move || {
                for j in 0..1024 * 1024 * 32 {
                    unpredictable_delay(500);
                    q.enqueue(j);
                }
            })
            .unwrap();

        writer.join().unwrap();
    }
}

fn unpredictable_delay(extra: usize) {}
