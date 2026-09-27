//! Drive the [`PosixFileSystem`] impl through the standard `FileSystem`
//! contract suite. Adding a new `FileSystem` impl in the future means adding
//! one more `#[tokio::test]` here that points at the same suite — that's the
//! whole shape of contract testing.

use platform_posix_minimal::PosixFileSystem;
use test_harness::contracts::filesystem::filesystem_contract_tests;

#[tokio::test]
async fn posix_filesystem_passes_contract() {
    let fs = PosixFileSystem::new(std::env::temp_dir());
    filesystem_contract_tests(&fs).await;
}
