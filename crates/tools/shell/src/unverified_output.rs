//! Model-facing description of an unverified shell observation.
//!
//! The allocation is a caller-provided hint, not evidence that the runner
//! used the file, that the file still exists, or that a process has exited.

pub(crate) fn unverified_output_message(
    reason: &str,
    allocation: Option<&(String, String)>,
) -> String {
    let output = match allocation {
        Some((id, path)) => format!(
            "The allocated output spool for {id:?} was {path:?}. It may be empty, incomplete, unavailable, or not used by the runner."
        ),
        None => "No output spool allocation was reported.".to_owned(),
    };
    format!(
        "{reason}\nShell termination is unverified. {output} A task record, a usable stop handle, and recovery after restart are not guaranteed."
    )
}

#[cfg(test)]
mod tests {
    use super::unverified_output_message;

    #[test]
    fn allocated_path_is_reported_without_claiming_exit_or_recovery() {
        let allocation = ("b123".to_owned(), "/tmp/b123.output".to_owned());
        let text = unverified_output_message("connection lost", Some(&allocation));
        assert!(text.contains("connection lost"));
        assert!(text.contains("\"b123\""));
        assert!(text.contains("\"/tmp/b123.output\""));
        assert!(text.contains("termination is unverified"));
        assert!(text.contains("not guaranteed"));
        assert!(!text.contains("process exited"));
    }

    #[test]
    fn missing_and_control_character_allocations_are_unambiguous() {
        let missing = unverified_output_message("failure", None);
        assert!(missing.contains("No output spool allocation was reported"));
        let allocation = ("first\nsecond".to_owned(), "path\nother".to_owned());
        let text = unverified_output_message("failure", Some(&allocation));
        assert!(text.contains("first\\nsecond"));
        assert!(text.contains("path\\nother"));
    }

    #[test]
    fn concurrent_calls_report_only_their_own_allocation() {
        let threads: Vec<_> = (0..32)
            .map(|i| {
                std::thread::spawn(move || {
                    let allocation = (format!("task-{i}"), format!("/spool/{i}"));
                    let text = unverified_output_message("unverified", Some(&allocation));
                    assert!(text.contains(&format!("\"task-{i}\"")));
                    assert!(text.contains(&format!("\"/spool/{i}\"")));
                    for other in 0..32 {
                        if i != other {
                            assert!(!text.contains(&format!("\"task-{other}\"")));
                            assert!(!text.contains(&format!("\"/spool/{other}\"")));
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
