use serde::{Deserialize, Serialize};
use std::{
    iter::Sum,
    ops::{Add, AddAssign, Mul, MulAssign, Sub, SubAssign},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
    Zlib,
    Lzo,
    Zstd,
    Other,
}

impl Compression {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zlib => "zlib",
            Self::Lzo => "lzo",
            Self::Zstd => "zstd",
            Self::Other => "other/preallocated",
        }
    }

    pub(crate) const fn from_raw(raw: u8, preallocated: bool) -> Self {
        match (raw, preallocated) {
            (1, false) => Self::Zlib,
            (2, false) => Self::Lzo,
            (3, false) => Self::Zstd,
            (0, false) => Self::None,
            _ => Self::Other,
        }
    }
}

/// `disk` and `uncompressed` count physical extents once per scan; `referenced` counts sharing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BtrfsUsage {
    pub disk: u64,
    pub uncompressed: u64,
    pub referenced: u64,
}

impl BtrfsUsage {
    pub fn ratio(self) -> Option<f64> {
        (self.uncompressed > 0).then(|| self.disk as f64 / self.uncompressed as f64)
    }
}

impl Add for BtrfsUsage {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            disk: self.disk + rhs.disk,
            uncompressed: self.uncompressed + rhs.uncompressed,
            referenced: self.referenced + rhs.referenced,
        }
    }
}

impl AddAssign for BtrfsUsage {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for BtrfsUsage {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            disk: self.disk.saturating_sub(rhs.disk),
            uncompressed: self.uncompressed.saturating_sub(rhs.uncompressed),
            referenced: self.referenced.saturating_sub(rhs.referenced),
        }
    }
}

impl SubAssign for BtrfsUsage {
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl Sum for BtrfsUsage {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), Add::add)
    }
}

impl From<u64> for BtrfsUsage {
    fn from(bytes: u64) -> Self {
        Self {
            disk: bytes,
            ..Self::default()
        }
    }
}

impl From<BtrfsUsage> for u64 {
    fn from(usage: BtrfsUsage) -> Self {
        usage.disk
    }
}

macro_rules! scale_usage {
    ($($ty:ty),*) => {$(
        impl Mul<$ty> for BtrfsUsage {
            type Output = Self;
            fn mul(self, rhs: $ty) -> Self {
                let rhs = rhs as u64;
                Self {
                    disk: self.disk * rhs,
                    uncompressed: self.uncompressed * rhs,
                    referenced: self.referenced * rhs,
                }
            }
        }
        impl Mul<BtrfsUsage> for $ty {
            type Output = BtrfsUsage;
            fn mul(self, rhs: BtrfsUsage) -> BtrfsUsage {
                rhs * self
            }
        }
        impl MulAssign<$ty> for BtrfsUsage {
            fn mul_assign(&mut self, rhs: $ty) {
                *self = *self * rhs;
            }
        }
    )*};
}

scale_usage!(u8, u16, u32, u64, usize);

impl parallel_disk_usage::size::Size for BtrfsUsage {
    type Inner = u64;
    type DisplayFormat = parallel_disk_usage::bytes_format::BytesFormat;
    type DisplayOutput = parallel_disk_usage::bytes_format::Output;

    fn display(self, format: Self::DisplayFormat) -> Self::DisplayOutput {
        format.format(self.disk)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtrfsReport {
    pub files: u64,
    pub unique_extents: u64,
    pub total: BtrfsUsage,
    pub by_compression: Vec<(Compression, BtrfsUsage)>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub size: u64,
    pub children: Vec<Node>,
    pub is_directory: bool,
    pub needs_sub_scan: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs: Option<BtrfsUsage>,
}

impl Node {
    pub fn new(name: impl Into<String>, own_size: u64, children: Vec<Node>) -> Self {
        let size = own_size + children.iter().map(|child| child.size).sum::<u64>();
        Self {
            name: name.into(),
            size,
            children,
            is_directory: true,
            needs_sub_scan: false,
            btrfs: None,
        }
    }

    pub fn leaf(name: impl Into<String>, size: u64) -> Self {
        Self {
            name: name.into(),
            size,
            children: Vec::new(),
            is_directory: false,
            needs_sub_scan: false,
            btrfs: None,
        }
    }

    /// Deduplicated hardlinks can leave a parent smaller than its children's sum.
    pub fn own_size(&self) -> u64 {
        let inner: u64 = self.children.iter().map(|child| child.size).sum();
        self.size.saturating_sub(inner)
    }

    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(Node::node_count).sum::<usize>()
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }
}

#[cfg(test)]
mod test {
    use super::Node;

    #[test]
    fn size_is_inclusive() {
        let node = Node::new(
            "root",
            4096,
            vec![Node::leaf("a", 1000), Node::leaf("b", 24)],
        );
        assert_eq!(node.size, 4096 + 1000 + 24);
        assert_eq!(node.own_size(), 4096);
        assert_eq!(node.node_count(), 3);
    }

    #[test]
    fn own_size_saturates_when_children_overflow() {
        let node = Node {
            name: "root".into(),
            size: 10,
            children: vec![Node::leaf("a", 100)],
            is_directory: true,
            needs_sub_scan: false,
            btrfs: None,
        };
        assert_eq!(node.own_size(), 0);
    }
}
