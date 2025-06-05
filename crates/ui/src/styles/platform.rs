/// The platform style to use when rendering UI.
///
/// This can be used to abstract over platform differences.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum PlatformStyle {
    /// Display in macOS style.
    Mac,
}

impl PlatformStyle {
    /// Returns the [`PlatformStyle`] for the current platform.
    pub const fn platform() -> Self {
        Self::Mac
    }
}
