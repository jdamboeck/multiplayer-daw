//! The guard has to notice an allocation, or the other tests prove nothing. This lives in its
//! own test binary because the violation counter is process-wide.

use mdaw_engine::alloc_guard::{self, GuardedAlloc};

#[global_allocator]
static ALLOC: GuardedAlloc = GuardedAlloc;

#[test]
fn allocation_inside_a_realtime_section_is_counted() {
    let outside = vec![1u8; 100];
    assert_eq!(alloc_guard::violations(), 0);
    {
        let _realtime = alloc_guard::enter();
        let inside = std::hint::black_box(vec![2u8; 100]);
        drop(inside);
        {
            let _nested = alloc_guard::enter();
        }
        // Still inside after the nested section ended.
        drop(std::hint::black_box(Box::new(3u64)));
    }
    assert_eq!(alloc_guard::violations(), 4, "two allocations and two frees");
    drop(outside);
    drop(std::hint::black_box(vec![4u8; 100]));
    assert_eq!(alloc_guard::violations(), 4);
}
