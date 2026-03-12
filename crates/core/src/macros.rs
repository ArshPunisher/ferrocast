//! Small local macros.

/// A tiny local stand-in for the `bitflags` crate: one more dependency for twelve lines
/// isn't worth it, and serde support stays explicit.
macro_rules! bitflags_like {
    ($(#[$m:meta])* $name:ident : $t:ty { $($flag:ident = $v:expr,)* }) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub $t);
        impl $name {
            $(pub const $flag: $name = $name($v);)*
            pub const fn empty() -> Self { $name(0) }
            pub const fn contains(self, other: $name) -> bool { self.0 & other.0 == other.0 }
            pub const fn union(self, other: $name) -> Self { $name(self.0 | other.0) }
            pub fn insert(&mut self, other: $name) { self.0 |= other.0; }
            /// Names of the set flags, for logs and the UI.
            pub fn names(self) -> Vec<&'static str> {
                let mut out = Vec::new();
                $(if self.contains($name::$flag) && $v != 0 { out.push(stringify!($flag)); })*
                out
            }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.names())
            }
        }
    };
}
pub(crate) use bitflags_like;
