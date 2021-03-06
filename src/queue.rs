use core::marker::PhantomData;
use core::mem::{self, align_of, size_of};
use core::ptr::{self, drop_in_place, null_mut};
use core::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::atomic::compiler_fence;
extern crate alloc;

const CACHE_LINE_SIZE: usize = 64;

#[cfg(debug_assertions)]
struct ReentrantGuard {
    in_section: AtomicBool,
}

#[cfg(debug_assertions)]
impl ReentrantGuard {
    fn new(in_section: &AtomicBool) -> Self {
        assert!(!in_section.load(Ordering::Relaxed), "Concurrent (or re-entrant) enqueue or dequeue operation detected (only one thread at a time may hold the producer or consumer role)");
        Self {
            in_section: AtomicBool::new(true),
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for ReentrantGuard {
    fn drop(&mut self) {
        self.in_section.store(false, Ordering::Relaxed);
    }
}

pub struct ReaderWriterQueue<T, const MAX_BLOCK_SIZE: usize = 512> {
    /// (Atomic) Elements are dequeued from this block
    front_block: AtomicPtr<Block>,
    cacheline_filler: [u8; CACHE_LINE_SIZE - size_of::<AtomicPtr<Block>>()],
    /// (Atomic) Elements are enqueued to this block
    tail_block: AtomicPtr<Block>,
    largest_block_size: usize,
    #[cfg(debug_assertions)]
    enqueuing: AtomicBool,
    #[cfg(debug_assertions)]
    dequeuing: AtomicBool,
    _phantom: PhantomData<T>,
}

unsafe impl<T: Send> Sync for ReaderWriterQueue<T> {}
unsafe impl<T: Send> Send for ReaderWriterQueue<T> {}

impl<T, const MAX_BLOCK_SIZE: usize> ReaderWriterQueue<T, MAX_BLOCK_SIZE> {
    pub fn new() -> Self {
        Self::with_size(15)
    }

    pub fn with_size(size: usize) -> Self {
        assert!(
            MAX_BLOCK_SIZE == ceil_to_pow2(MAX_BLOCK_SIZE),
            "MAX_BLOCK_SIZE must be a power of 2"
        );
        assert!(MAX_BLOCK_SIZE >= 2, "MAX_BLOCK_SIZE must be at least 2");

        let mut first_block: *mut Block = null_mut();
        let mut largest_block_size = ceil_to_pow2(size + 1); // We need a spare slot to fit size elements in the block
        if largest_block_size > MAX_BLOCK_SIZE * 2 {
            // We need a spare block in case the producer is writing to a different block the consumer is reading from, and
            // wants to enqueue the maximum number of elements. We also need a spare element in each block to avoid the ambiguity
            // between front == tail meaning "empty" and "full".
            // So the effective number of slots that are guaranteed to be usable at any time is the block size - 1 times the
            // number of blocks - 1. Solving for size and applying a ceiling to the division gives us (after simplifying):
            let initial_block_count = (size + MAX_BLOCK_SIZE * 2 - 3) / (MAX_BLOCK_SIZE - 1);
            largest_block_size = MAX_BLOCK_SIZE;
            let mut last_block: *mut Block = null_mut();
            for i in 0..initial_block_count {
                let block = Self::make_block(largest_block_size);
                if first_block.is_null() {
                    first_block = block;
                } else {
                    unsafe { (*last_block).next = AtomicPtr::new(block) };
                }
                last_block = block;
                unsafe { (*block).next = AtomicPtr::new(first_block) };
            }
        } else {
            first_block = Self::make_block(largest_block_size);
            unsafe { (*first_block).next = AtomicPtr::new(first_block) };
        }
        fence(Ordering::SeqCst);
        Self {
            front_block: AtomicPtr::new(first_block),
            tail_block: AtomicPtr::new(first_block),
            cacheline_filler: [0; CACHE_LINE_SIZE - size_of::<AtomicPtr<Block>>()],
            largest_block_size,
            #[cfg(debug_assertions)]
            enqueuing: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            dequeuing: AtomicBool::new(false),
            _phantom: PhantomData,
        }
    }

    pub fn from_other(other: &mut Self) -> Self {
        let item = Self {
            front_block: AtomicPtr::new(other.front_block.load(Ordering::Relaxed)),
            tail_block: AtomicPtr::new(other.tail_block.load(Ordering::Relaxed)),
            cacheline_filler: [0; CACHE_LINE_SIZE - size_of::<AtomicPtr<Block>>()],
            largest_block_size: other.largest_block_size,
            #[cfg(debug_assertions)]
            enqueuing: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            dequeuing: AtomicBool::new(false),
            _phantom: PhantomData,
        };

        other.largest_block_size = 32;
        let mut b = Self::make_block(other.largest_block_size);
        unsafe { (*b).next = AtomicPtr::new(b) };
        other.front_block = AtomicPtr::new(b);
        other.tail_block = AtomicPtr::new(b);

        item
    }

    pub fn try_enqueue(&self, element: T) -> bool {
        self.inner_enqueue(element, false)
    }

    pub fn enqueue(&self, element: T) -> bool {
        self.inner_enqueue(element, true)
    }

    pub fn try_dequeue(&self) -> Option<T> {
        let front_block_ = self.front_block.load(Ordering::Relaxed);
        let front_block = unsafe { front_block_.as_mut().unwrap() };
        let block_tail = front_block.local_tail;
        let mut block_front = front_block.front.load(Ordering::Relaxed);

        let result;
        if block_front != block_tail
            || block_front != {
                front_block.local_tail = front_block.tail.load(Ordering::Relaxed);
                front_block.local_tail
            }
        {
            fence(Ordering::Acquire);

            // non_empty_front_block;
            {
                let element =
                    unsafe { front_block.data.add(block_front * size_of::<T>()) as *mut T };
                result = Some(unsafe { ptr::read(element) });
                unsafe { drop_in_place(element) };

                block_front = (block_front + 1) & front_block.size_mask;

                fence(Ordering::Release);
                front_block.front = AtomicUsize::new(block_front);
            }
        } else if front_block_ != self.tail_block.load(Ordering::Relaxed) {
            fence(Ordering::Acquire);

            let front_block_ = self.front_block.load(Ordering::Relaxed);
            front_block.local_tail = front_block.tail.load(Ordering::Relaxed);
            let block_tail = front_block.local_tail;
            fence(Ordering::Acquire);

            if block_front != block_tail {
                // goto non_empty_front_block;
                let element =
                    unsafe { front_block.data.add(block_front * size_of::<T>()) as *mut T };
                result = Some(unsafe { ptr::read(element) });
                unsafe { drop_in_place(element) };

                block_front = (block_front + 1) & front_block.size_mask;

                fence(Ordering::Release);
                front_block.front = AtomicUsize::new(block_front);

                return result;
            }

            let next_block_ = front_block.next.load(Ordering::Relaxed);
            let next_block = unsafe { next_block_.as_mut().unwrap() };
            let next_block_front = next_block.front.load(Ordering::Relaxed);
            next_block.local_tail = next_block.tail.load(Ordering::Relaxed);
            let next_block_tail = next_block.local_tail;
            fence(Ordering::Acquire);

            assert!(next_block_front != next_block_tail);

            fence(Ordering::Release);
            let front_block = next_block;
            self.front_block.store(front_block, Ordering::Relaxed);

            compiler_fence(Ordering::Release);

            let element =
                unsafe { front_block.data.add(next_block_front * size_of::<T>()) as *mut T };

            result = Some(unsafe { ptr::read(element) });
            unsafe { drop_in_place(element) };

            let next_block_front = (next_block_front + 1) & front_block.size_mask;

            fence(Ordering::Release);
            front_block.front.store(next_block_front, Ordering::Relaxed);
        } else {
            result = None;
        }
        result
    }

    fn peek(&self) -> Option<&T> {
        #[cfg(debug_assertions)]
        let _guard = ReentrantGuard::new(&self.dequeuing);

        let front_block_ = self.front_block.load(Ordering::Relaxed);
        let front_block = unsafe { front_block_.as_mut() }.unwrap();
        let block_tail = front_block.local_tail;
        let block_front = front_block.front.load(Ordering::Relaxed);

        if block_front != block_tail
            || block_front != {
                front_block.local_tail = front_block.tail.load(Ordering::Relaxed);
                front_block.local_tail
            }
        {
            fence(Ordering::Acquire);
            return unsafe {
                (front_block.data.add(block_front * size_of::<T>()) as *mut T).as_ref()
            };
        } else if front_block_ != self.tail_block.load(Ordering::Relaxed) {
            fence(Ordering::Acquire);
            let front_block = self.front_block.load(Ordering::Relaxed);
            let front_block = unsafe { front_block.as_mut() }.unwrap();
            front_block.local_tail = front_block.tail.load(Ordering::Relaxed);
            let block_tail = front_block.local_tail;
            let block_front = front_block.front.load(Ordering::Relaxed);
            fence(Ordering::Acquire);

            if block_front != block_tail {
                return unsafe {
                    (front_block.data.add(block_front * size_of::<T>()) as *mut T).as_ref()
                };
            }

            let next_block = front_block.next.load(Ordering::Relaxed);
            let next_block = unsafe { next_block.as_mut() }.unwrap();

            let next_block_front = next_block.front.load(Ordering::Relaxed);
            fence(Ordering::Acquire);

            assert!(next_block_front != next_block.tail.load(Ordering::Relaxed));
            return unsafe {
                (next_block.data.add(next_block_front * size_of::<T>()) as *mut T).as_ref()
            };
        }

        None
    }

    fn pop(&self) -> bool {
        #[cfg(debug_assertions)]
        let _guard = ReentrantGuard::new(&self.dequeuing);

        true
    }

    #[inline]
    pub fn size_approx(&self) -> usize {
        let mut result = 0;
        let front_block = self.front_block.load(Ordering::Relaxed);
        let mut block = front_block;
        loop {
            fence(Ordering::Acquire);
            let block_front = unsafe { (*block).front.load(Ordering::Relaxed) };
            let block_tail = unsafe { (*block).tail.load(Ordering::Relaxed) };
            result += (block_tail - block_front) & unsafe { (*block).size_mask };
            block = unsafe { (*block).next.load(Ordering::Relaxed) };
            if block == front_block {
                break;
            }
        }
        result
    }

    #[inline]
    pub fn max_capacity(&self) -> usize {
        let mut result = 0;
        let front_block = self.front_block.load(Ordering::Relaxed);
        let mut block = front_block;
        loop {
            fence(Ordering::Acquire);
            result += unsafe { (*block).size_mask };
            block = unsafe { (*block).next.load(Ordering::Relaxed) };
            if block == front_block {
                break;
            }
        }
        result
    }

    fn inner_enqueue(&self, element: T, can_alloc: bool) -> bool {
        #[cfg(debug_assertions)]
        let _guard = ReentrantGuard::new(&self.enqueuing);


        true
    }

    fn make_block(capacity: usize) -> *mut Block {
        let mut size = size_of::<Block>() + align_of::<Block>() - 1;
        size += size_of::<T>() + align_of::<T>() - 1;
        let new_block_raw =
            unsafe { alloc::alloc::alloc(core::alloc::Layout::array::<u8>(size).unwrap()) };
        let new_block_aligned = unsafe { align_for::<Block>(new_block_raw) };
        let new_block_data =
            unsafe { align_for::<Block>(new_block_aligned.add(size_of::<Block>())) };
        Box::new(Block::new(capacity, new_block_raw, new_block_data)).as_mut()
    }
}

impl<T, const MAX_BLOCK_SIZE: usize> Drop for ReaderWriterQueue<T, MAX_BLOCK_SIZE> {
    fn drop(&mut self) {
        fence(Ordering::SeqCst);
        let front_block = self.front_block.load(Ordering::Relaxed);
        let mut block = front_block.clone();
        loop {
            let next_block = unsafe { (*block).next.load(Ordering::Relaxed) };
            let block_front = unsafe { (*block).front.load(Ordering::Relaxed) };
            let block_tail = unsafe { (*block).tail.load(Ordering::Relaxed) };
            let mut i = block_front;
            while i != block_tail {
                let element = unsafe { (*block).data.add(i * size_of::<T>()) as *mut T };
                unsafe { drop_in_place(element) };
                i = (i + 1) & unsafe { (*block).size_mask };
            }
            let raw_block = unsafe { (*block).raw_this };
            drop(block);
            drop(raw_block);
            block = next_block;
            if block == front_block {
                break;
            }
        }
    }
}

struct Block {
    front: AtomicUsize,
    local_tail: usize,
    cacheline_filler0: [u8; CACHE_LINE_SIZE - size_of::<AtomicUsize>() - size_of::<usize>()],

    tail: AtomicUsize,
    local_front: usize,
    cacheline_filler1: [u8; CACHE_LINE_SIZE - size_of::<AtomicUsize>() - size_of::<usize>()],

    next: AtomicPtr<Block>,
    data: *mut u8,
    size_mask: usize,

    pub raw_this: *mut u8,
}

impl Block {
    pub fn new(size: usize, raw_this: *mut u8, data: *mut u8) -> Block {
        Block {
            front: AtomicUsize::new(0),
            local_tail: 0,
            cacheline_filler0: [0; CACHE_LINE_SIZE - size_of::<AtomicUsize>() - size_of::<usize>()],
            tail: AtomicUsize::new(0),
            local_front: 0,
            cacheline_filler1: [0; CACHE_LINE_SIZE - size_of::<AtomicUsize>() - size_of::<usize>()],
            next: AtomicPtr::new(null_mut()),
            data,
            size_mask: size - 1,
            raw_this,
        }
    }
}

#[inline(always)]
unsafe fn align_for<U>(ptr: *mut u8) -> *mut u8 {
    let alignment = align_of::<U>();
    let alignment = (alignment - ptr as usize % alignment) % alignment;
    ptr.add(alignment)
}

fn ceil_to_pow2(x: usize) -> usize {
    let mut x = x;
    // From http://graphics.stanford.edu/~seander/bithacks.html#RoundUpPowerOf2
    x -= 1;
    x |= x >> 1;
    x |= x >> 2;
    x |= x >> 4;
    let mut i = 1;
    while i < size_of::<usize>() {
        x |= x >> (i << 3);
        i <<= 1;
    }
    x += 1;
    return x;
}
