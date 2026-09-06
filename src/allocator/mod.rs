//! Platform-selected heap allocator type.

#[cfg(target_os = "windows")]
pub(crate) use mimalloc::MiMalloc as Allocator;

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub(crate) use jemallocator::Jemalloc as Allocator;

#[cfg(not(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
pub(crate) use std::alloc::System as Allocator;

#[allow(dead_code)]
pub(crate) fn allocator_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "mimalloc"
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        "jemalloc"
    }

    #[cfg(not(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )))]
    {
        "system"
    }
}

#[cfg(test)]
mod tests {
    use super::allocator_name;

    #[test]
    fn selects_allocator_for_target_platform() {
        #[cfg(target_os = "windows")]
        assert_eq!(allocator_name(), "mimalloc");

        #[cfg(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        assert_eq!(allocator_name(), "jemalloc");

        #[cfg(not(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        )))]
        assert_eq!(allocator_name(), "system");
    }
}
