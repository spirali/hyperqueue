use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::common::arrayparser::parse_array;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct IntRange {
    pub start: u32,
    pub count: u32,
    pub step: u32,
}

impl IntRange {
    pub fn new(start: u32, count: u32, step: u32) -> IntRange {
        IntRange { start, count, step }
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + use<> {
        (self.start..self.start + self.count).step_by(self.step as usize)
    }

    pub fn contains(&self, value: u32) -> bool {
        let end = self.start + self.count;
        self.start <= value && value < end && ((value - self.start) % self.step == 0)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IntArray {
    ranges: Vec<IntRange>,
}

impl IntArray {
    pub fn new_empty() -> IntArray {
        Self::new(Vec::new())
    }

    pub fn new(ranges: Vec<IntRange>) -> IntArray {
        IntArray { ranges }
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn from_sorted_ids(ids: impl Iterator<Item = u32>) -> IntArray {
        let mut ranges: Vec<IntRange> = Vec::new();
        let mut last_id = None;
        for id in ids {
            debug_assert!(last_id.map(|last_id| last_id < id).unwrap_or(true));
            if last_id.map(|last_id| last_id + 1 == id).unwrap_or(false) {
                ranges.last_mut().unwrap().count += 1;
            } else {
                ranges.push(IntRange::new(id, 1, 1));
            }
            last_id = Some(id);
        }
        IntArray { ranges }
    }
    pub fn from_id(id: u32) -> IntArray {
        Self::from_sorted_ids([id].into_iter())
    }

    pub fn from_range(start: u32, count: u32) -> Self {
        IntArray {
            ranges: vec![IntRange {
                start,
                count,
                step: 1,
            }],
        }
    }

    #[inline]
    pub fn id_count(&self) -> u32 {
        self.ranges.iter().map(|x| x.count).sum()
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.ranges.iter().flat_map(|x| x.iter())
    }

    #[inline]
    pub fn contains(&self, id: u32) -> bool {
        self.ranges.iter().any(|range| range.contains(id))
    }
}

impl FromStr for IntArray {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_array(s)
    }
}

impl fmt::Display for IntArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, x) in self.ranges.iter().enumerate() {
            if idx > 0 {
                write!(f, ",")?;
            }
            if x.count == 1 {
                write!(f, "{}", x.start)?;
            } else if x.step == 1 {
                write!(f, "{}-{}", x.start, x.start + x.count - 1)?;
            } else {
                write!(f, "{}-{}:{}", x.start, x.start + x.count - 1, x.step)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::common::arraydef::IntRange;

    #[test]
    fn range_iterate() {
        assert_eq!(
            IntRange::new(1, 5, 1).iter().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            IntRange::new(2, 9, 3).iter().collect::<Vec<_>>(),
            vec![2, 5, 8]
        );
        assert_eq!(
            IntRange::new(2, 10, 3).iter().collect::<Vec<_>>(),
            vec![2, 5, 8, 11]
        );
    }

    #[test]
    fn range_contains() {
        assert!(!IntRange::new(1, 5, 1).contains(0));
        assert!(IntRange::new(1, 5, 1).contains(1));
        assert!(IntRange::new(1, 5, 1).contains(2));
        assert!(IntRange::new(1, 5, 1).contains(5));
        assert!(!IntRange::new(1, 5, 1).contains(6));

        assert!(IntRange::new(1, 8, 3).contains(1));
        assert!(!IntRange::new(1, 8, 3).contains(2));
        assert!(!IntRange::new(1, 8, 3).contains(3));
        assert!(IntRange::new(1, 8, 3).contains(4));
        assert!(!IntRange::new(1, 8, 3).contains(5));
        assert!(!IntRange::new(1, 8, 3).contains(6));
        assert!(IntRange::new(1, 8, 3).contains(7));
        assert!(!IntRange::new(1, 8, 3).contains(8));
    }
}
