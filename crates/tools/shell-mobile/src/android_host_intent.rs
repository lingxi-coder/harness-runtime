//! Android-host command/path advisory for the mobile Shell tool.
//!
//! The mobile Shell is an app-sandboxed workspace shell, not `adb shell` and
//! not Android's privileged `shell` UID. Refuse obvious attempts to control the
//! host or escape into shared/system storage before execution, so the model
//! receives one actionable error instead of pages of SELinux `Access denied`
//! diagnostics. The Android app sandbox remains the actual security boundary.

use crate::net_intent::split_segments;

const ANDROID_HOST_COMMANDS: &[&str] = &[
    "am", "appops", "cmd", "content", "dumpsys", "getprop", "input", "monkey", "pm", "settings",
    "su", "svc",
];

const ANDROID_HOST_PATHS: &[&str] = &[
    "/sdcard",
    "/storage/emulated",
    "/data/data",
    "/data/user",
    "/system",
    "/vendor",
    "/product",
    "/apex",
];

/// Return an actionable model-facing error for an obvious Android host action.
#[must_use]
pub(crate) fn android_host_intent(command: &str, computer_use_available: bool) -> Option<String> {
    for segment in split_segments(command) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let Some(head) = command_head(&words) else {
            continue;
        };
        if ANDROID_HOST_COMMANDS.contains(&head) {
            let guidance = if computer_use_available {
                "Use `android_use` for Android UI/app operations: call `status` first, then \
                 `open_app`/UI actions only inside a user-started, authorized Computer Use \
                 session."
            } else {
                "Android Computer Use is unavailable in this build; ask the user to complete \
                 the Android UI/app operation manually."
            };
            return Some(format!(
                "`{head}` is an Android adb/system-shell command and cannot run from the \
                 app-sandboxed Shell. {guidance} Never hide this failure with `|| true` or \
                 stderr redirection."
            ));
        }
        for word in words {
            let candidate = normalized_path_token(word);
            if ANDROID_HOST_PATHS
                .iter()
                .any(|prefix| candidate == *prefix || candidate.starts_with(&format!("{prefix}/")))
            {
                return Some(format!(
                    "`{candidate}` is outside the app-private Project workspace. Use relative \
                     workspace paths for Shell/File tools. Import or export external Android \
                     folders through the Project SAF UI{}.",
                    if computer_use_available {
                        "; use `android_use` for app/UI actions"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    None
}

fn command_head<'a>(words: &'a [&'a str]) -> Option<&'a str> {
    let mut index = 0;
    while index < words.len() && words[index].contains('=') {
        index += 1;
    }
    while matches!(
        words.get(index).copied(),
        Some("command" | "exec" | "nohup")
    ) {
        index += 1;
    }
    let raw = *words.get(index)?;
    let cleaned = raw.trim_matches(|c: char| {
        !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '/' && c != '.'
    });
    Some(cleaned.rsplit('/').next().unwrap_or(cleaned))
}

fn normalized_path_token(word: &str) -> &str {
    let token = word
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';' | ')' | '('))
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '>' | '<'));
    token.rsplit('=').next().unwrap_or(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_android_host_control_commands() {
        for command in [
            "monkey -p com.android.chrome 1",
            "cmd activity start-activity -p com.android.chrome",
            "/system/bin/am start https://example.com",
            "echo ok && input tap 10 20",
            "FOO=1 pm list packages",
        ] {
            let error = android_host_intent(command, true)
                .unwrap_or_else(|| panic!("{command:?} should be refused"));
            assert!(error.contains("android_use"), "{error}");
        }
    }

    #[test]
    fn refuses_android_host_paths() {
        for command in [
            "cat /sdcard/Download/a.html",
            "base64 index.html >/storage/emulated/0/Download/a.b64",
            "ls /data/user/0",
            "cat x 2>/system/tmp/error",
        ] {
            assert!(
                android_host_intent(command, true).is_some(),
                "{command:?} should be refused"
            );
        }
    }

    #[test]
    fn permits_workspace_commands_and_words_used_as_data() {
        for command in [
            "cat index.html",
            "mkdir -p dist && cp index.html dist/",
            "printf '%s' monkey > notes.txt",
            "python3 -m compileall .",
        ] {
            assert!(
                android_host_intent(command, true).is_none(),
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn unavailable_build_does_not_advertise_missing_android_use_tool() {
        let error = android_host_intent("monkey -p com.android.chrome 1", false)
            .expect("host command should be refused");
        assert!(error.contains("unavailable in this build"), "{error}");
        assert!(!error.contains("`android_use`"), "{error}");
    }
}
