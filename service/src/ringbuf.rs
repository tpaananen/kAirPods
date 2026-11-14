use std::fmt;

#[derive(Clone, Copy)]
pub struct Ring<T: Default + Copy, const N: usize> {
   data: [T; N],
   tail: usize, // Increments without bound, wraps only when indexing
}

impl<T: Default + Copy, const N: usize> Ring<T, N> {
   /// Constructs an empty ring buffer.
   pub fn new() -> Self {
      Self {
         data: [T::default(); N],
         tail: 0,
      }
   }

   /// Current number of elements.
   #[inline]
   pub const fn len(&self) -> usize {
      if self.tail > N { N } else { self.tail }
   }

   /// `true` if the buffer is empty.
   #[inline]
   pub const fn is_empty(&self) -> bool {
      self.tail == 0
   }

   /// Calculate the head position (oldest element).
   #[inline]
   const fn head(&self) -> usize {
      if self.tail > N {
         (self.tail - N) % N
      } else {
         0
      }
   }

   /// Logical-to-physical index mapping.
   #[inline]
   const fn phys_index(&self, logical: usize) -> usize {
      debug_assert!(logical < self.len());
      (self.head() + logical) % N
   }

   /// Push a value to the **back** (newest side) of the buffer.
   pub const fn push(&mut self, value: T) {
      self.data[self.tail % N] = value;
      self.tail += 1;
   }

   /// Clears the buffer.
   pub const fn clear(&mut self) {
      self.tail = 0;
   }

   /// Get read-only access to element at `index` (0 = oldest).
   #[inline]
   pub const fn get(&self, index: usize) -> Option<&T> {
      if index >= self.len() {
         None
      } else {
         Some(&self.data[self.phys_index(index)])
      }
   }

   /// Newest element.
   #[inline]
   pub const fn last(&self) -> Option<&T> {
      if self.is_empty() {
         None
      } else {
         self.get(self.len() - 1)
      }
   }

   /// Returns the buffer contents as a pair of slices.
   /// The first slice contains the older elements, the second the newer ones.
   /// The slices are in logical order from oldest to newest.
   pub fn as_slices(&self) -> (&[T], &[T]) {
      if self.tail <= N {
         // Not wrapped yet: elements sit at the start in insertion order
         (&self.data[..self.tail], &[])
      } else {
         // Wrapped: the oldest element is at `head`, the newest right before it
         let head = self.head();
         (&self.data[head..], &self.data[..head])
      }
   }

   /// Iterator from oldest to newest.
   pub fn iter(&self) -> RingIter<'_, T> {
      let (left, right) = self.as_slices();
      RingIter { left, right }
   }
}

impl<T: Default + Copy, const N: usize> Extend<T> for Ring<T, N> {
   fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
      for item in iter {
         self.push(item);
      }
   }
}

/// Iterator over immutable references.
#[derive(Clone)]
pub struct RingIter<'a, T: Default + Copy> {
   left: &'a [T],
   right: &'a [T],
}

impl<'a, T: Default + Copy> Iterator for RingIter<'a, T> {
   type Item = &'a T;

   fn next(&mut self) -> Option<Self::Item> {
      if let Some((first, rest)) = self.left.split_first() {
         self.left = rest;
         Some(first)
      } else if let Some((first, rest)) = self.right.split_first() {
         self.right = rest;
         Some(first)
      } else {
         None
      }
   }

   fn size_hint(&self) -> (usize, Option<usize>) {
      let remaining = self.left.len() + self.right.len();
      (remaining, Some(remaining))
   }
}

impl<T: Default + Copy> ExactSizeIterator for RingIter<'_, T> {
   fn len(&self) -> usize {
      self.left.len() + self.right.len()
   }
}

impl<'a, T: Default + Copy, const N: usize> IntoIterator for &'a Ring<T, N> {
   type Item = &'a T;
   type IntoIter = RingIter<'a, T>;

   fn into_iter(self) -> Self::IntoIter {
      self.iter()
   }
}

impl<T: Default + Copy + fmt::Debug, const N: usize> fmt::Debug for Ring<T, N> {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_list().entries(self.iter()).finish()
   }
}

impl<T: Default + Copy, const N: usize> Default for Ring<T, N> {
   fn default() -> Self {
      Self::new()
   }
}

impl<T: Default + Copy, const N: usize> FromIterator<T> for Ring<T, N> {
   fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
      let mut buffer = Self::new();
      buffer.extend(iter);
      buffer
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn basics_push_and_overwrite() {
      let mut rb: Ring<i32, 3> = Ring::new();
      rb.push(1);
      rb.push(2);
      rb.push(3);
      rb.push(4); // overwrites 1
      assert_eq!(rb.len(), 3);
      assert_eq!(rb.last(), Some(&4));
   }

   #[test]
   fn tail_increments_beyond_n() {
      let mut rb: Ring<i32, 4> = Ring::new();
      rb.push(1);
      rb.push(2);
      rb.push(3);
      rb.push(4); // tail = 4
      rb.push(5); // tail = 5
      assert_eq!(rb.tail, 5);
      assert_eq!(rb.len(), 4);
   }

   #[test]
   fn clear_resets_state_and_reuse() {
      let mut rb: Ring<u8, 2> = Ring::new();
      rb.push(10);
      rb.push(20);
      rb.clear();
      assert!(rb.is_empty());
      assert_eq!(rb.len(), 0);
      assert_eq!(rb.tail, 0);

      // should be reusable after clear
      rb.push(30);
      assert_eq!(rb.last(), Some(&30));
   }

   #[test]
   fn as_slices_contiguous() {
      let mut rb: Ring<i32, 5> = Ring::new();
      rb.push(1);
      rb.push(2);
      rb.push(3);

      let (left, right) = rb.as_slices();
      assert_eq!(left, &[1, 2, 3]);
      assert!(right.is_empty());
   }

   #[test]
   fn as_slices_wrapped() {
      let mut rb: Ring<i32, 4> = Ring::new();
      rb.push(1);
      rb.push(2);
      rb.push(3);
      rb.push(4);
      rb.push(5); // Overwrites 1, wraps around

      let (left, right) = rb.as_slices();
      assert_eq!(left, &[2, 3, 4]);
      assert_eq!(right, &[5]);
   }

   #[test]
   fn as_slices_exactly_full() {
      let mut rb: Ring<i32, 4> = Ring::new();
      for i in 1..=4 {
         rb.push(i);
      } // tail == N: full, but nothing overwritten yet

      let (left, right) = rb.as_slices();
      assert_eq!(left, &[1, 2, 3, 4]);
      assert!(right.is_empty());
      assert_eq!(rb.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3, 4]);

      // Full again after wrapping around completely
      for i in 5..=8 {
         rb.push(i);
      }
      assert_eq!(rb.iter().copied().collect::<Vec<_>>(), vec![5, 6, 7, 8]);
   }

   #[test]
   fn as_slices_empty() {
      let rb: Ring<i32, 5> = Ring::new();
      let (left, right) = rb.as_slices();
      assert!(left.is_empty());
      assert!(right.is_empty());
   }

   #[test]
   fn iterator_with_slices() {
      let mut rb: Ring<i32, 4> = Ring::new();
      for i in 1..=6 {
         rb.push(i);
      }
      // Buffer contains [3, 4, 5, 6]

      let collected: Vec<i32> = rb.iter().copied().collect();
      assert_eq!(collected, vec![3, 4, 5, 6]);
   }
}
