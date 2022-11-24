use persistent_range_query::naive::*;
use persistent_range_query::ops::rsq::*;
use persistent_range_query::ops::SameElementsInitializer;
use persistent_range_query::segment_tree::{MidpointableKey, PersistentSegmentTree};
use persistent_range_query::{PersistentVecStorage, VecReadableVersion};
use std::ops::Range;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct K(u8);

impl IndexableKey for K {
    fn index(all_keys: &Range<Self>, key: &Self) -> usize {
        (key.0 as usize) - (all_keys.start.0 as usize)
    }

    fn element_range(all_keys: &Range<Self>, index: usize) -> Range<Self> {
        K(all_keys.start.0 + index as u8)..K(all_keys.start.0 + index as u8 + 1)
    }
}

impl SumOfSameElements<K> for i32 {
    fn sum(initial_element_value: &Self, keys: &Range<K>) -> Self {
        initial_element_value * (keys.end.0 - keys.start.0) as Self
    }
}

impl MidpointableKey for K {
    fn midpoint(range: &Range<Self>) -> Self {
        K(range.start.0 + (range.end.0 - range.start.0) / 2)
    }
}

fn test_storage<
    S: PersistentVecStorage<AddAssignModification<i32>, SameElementsInitializer<i32>, K>,
>() {
    let mut s = S::new(K(0)..K(12), SameElementsInitializer::new(0i32));
    assert_eq!(*s.get(K(0)..K(12)).sum(), 0);

    s.modify(K(2)..K(5), AddAssignModification::Add(3));
    assert_eq!(*s.get(K(0)..K(12)).sum(), 3 + 3 + 3);
    let s_old = s.freeze();

    s.modify(K(3)..K(6), AddAssignModification::Assign(10));
    assert_eq!(*s.get(K(0)..K(12)).sum(), 3 + 10 + 10 + 10);

    s.modify(K(4)..K(7), AddAssignModification::Add(2));
    assert_eq!(*s.get(K(0)..K(12)).sum(), 3 + 10 + 12 + 12 + 2);

    assert_eq!(*s.get(K(4)..K(6)).sum(), 12 + 12);
    assert_eq!(*s_old.get(K(4)..K(6)).sum(), 3);
}

#[test]
fn test_naive() {
    test_storage::<NaiveVecStorage<_, _, _>>();
}

#[test]
fn test_segment_tree() {
    test_storage::<PersistentSegmentTree<_, _, _>>();
}
