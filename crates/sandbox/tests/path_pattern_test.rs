use sandbox::path_pattern::resolve_path_pattern_for_sandbox;
use std::path::PathBuf;

#[test]
fn double_slash_strips_one_slash() {
    let out = resolve_path_pattern_for_sandbox("//etc/passwd", &PathBuf::from("/home/u/.lingxi"));
    assert_eq!(out, "/etc/passwd");
}

#[test]
fn double_slash_works_with_glob() {
    let out = resolve_path_pattern_for_sandbox("//.aws/**", &PathBuf::from("/home/u/.lingxi"));
    assert_eq!(out, "/.aws/**");
}

#[test]
fn single_slash_resolves_against_settings_dir() {
    let out = resolve_path_pattern_for_sandbox("/foo/**", &PathBuf::from("/home/u/.lingxi"));
    assert_eq!(out, "/home/u/.lingxi/foo/**");
}

#[test]
fn tilde_passes_through() {
    let out = resolve_path_pattern_for_sandbox("~/Documents", &PathBuf::from("/anything"));
    assert_eq!(out, "~/Documents");
}

#[test]
fn dot_relative_passes_through() {
    let out = resolve_path_pattern_for_sandbox("./build", &PathBuf::from("/anything"));
    assert_eq!(out, "./build");
}

#[test]
fn bare_path_passes_through() {
    let out = resolve_path_pattern_for_sandbox("target", &PathBuf::from("/anything"));
    assert_eq!(out, "target");
}
