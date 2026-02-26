/// Make a fixed size array index start at one (instead of zero).
/// This is a common pattern within XHCI for port and slot IDs, and
/// manually handling the difference is error-prone.
#[derive(Debug)]
pub struct OneIndexed<T, const S: usize> {
    array: [T; S],
}

impl<T, const S: usize> OneIndexed<T, S> {
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.array.iter()
    }
    #[allow(unused)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.array.iter_mut()
    }
    pub fn get(&self, index: usize) -> Option<&T> {
        self.array.get(index.wrapping_sub(1))
    }
    /// Enumerating elements with correct index.
    ///
    /// Using some_one_indexed.iter().enumerate() generates an iterator like
    /// (0, some_one_indexed[1]), (1, some_one_indexed[2]), ...
    /// some_one_indexed.enumerate instead generates an iterator like
    /// (1, some_one_indexed[1]), (2, some_one_indexed[2]), ...
    ///
    /// This method is useful for avoiding manual "one-shifting" when trying to
    /// filter for the indices of items with specific properties.
    pub fn enumerate(&self) -> impl Iterator<Item = (usize, &T)> {
        self.array.iter().enumerate().map(|(i, e)| (i + 1, e))
    }
}
impl<T, const S: usize> std::convert::From<[T; S]> for OneIndexed<T, S> {
    fn from(val: [T; S]) -> Self {
        Self { array: val }
    }
}
impl<T, const S: usize> std::ops::Index<usize> for OneIndexed<T, S> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.array[index.wrapping_sub(1)]
    }
}
impl<T, const S: usize> std::ops::IndexMut<usize> for OneIndexed<T, S> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.array[index.wrapping_sub(1)]
    }
}
