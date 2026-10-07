//! Dense, typed ids. Each is a `u32` index into the columns of one table.

macro_rules! dense_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
        #[repr(transparent)]
        pub struct $name(pub u32);

        impl $name {
            /// Sentinel meaning "no id".
            pub const NONE: Self = Self(u32::MAX);
            #[inline(always)]
            pub const fn idx(self) -> usize { self.0 as usize }
            #[inline(always)]
            pub const fn is_none(self) -> bool { self.0 == u32::MAX }
            #[inline(always)]
            pub const fn is_some(self) -> bool { self.0 != u32::MAX }
            #[inline(always)]
            pub fn from_idx(i: usize) -> Self {
                debug_assert!(i < u32::MAX as usize);
                Self(i as u32)
            }
        }
    };
}

dense_id!(
    /// Index into [`crate::NodeTable`].
    NodeId
);
dense_id!(
    /// Index into [`crate::EdgeTable`].
    EdgeId
);
dense_id!(
    /// Index into [`crate::FileTable`].
    FileId
);
dense_id!(
    /// Index into [`crate::CommentTable`].
    CommentId
);
