//! Native Handback inbox substitution. Full report state is stored separately;
//! only this model-facing copy may become a persisted-output pointer.

use lingxi_core::host::{FileSystem, FsError};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const REPORT_THRESHOLD_UTF16: usize = 50_000;

pub struct PersistedHandbackBody {
    pub text: Utf16JsonProjection,
}
const PREVIEW_UTF16: usize = 2_000;
const MAX_PERSIST_UTF16: usize = 1_073_741_824;

/// Host-provided transcript filesystem and actual session directory. The model
/// never selects this root or a path outside its generated tool-results leaf.
pub struct HandbackReportOutput {
    pub fs: Arc<dyn FileSystem>,
    pub session_dir: PathBuf,
}

impl HandbackReportOutput {
    pub async fn substitute(
        &self,
        text: &str,
        tool_use_id: Option<&str>,
    ) -> Option<PersistedHandbackBody> {
        self.substitute_utf16(&text.encode_utf16().collect::<Vec<_>>(), tool_use_id)
            .await
    }

    /// Substitute one report using the exact model-provided UTF-16 code units.
    /// The filesystem sink follows JavaScript's UTF-8 replacement behavior for
    /// lone surrogates, while the model-facing preview remains exact.
    pub async fn substitute_utf16(
        &self,
        text: &[u16],
        tool_use_id: Option<&str>,
    ) -> Option<PersistedHandbackBody> {
        let original_size = text.len();
        if original_size <= REPORT_THRESHOLD_UTF16 {
            return None;
        }
        let leaf = tool_use_id
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 200
                    && *id != "."
                    && *id != ".."
                    && id.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                    })
            })
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "SubagentHandback-{}",
                    lingxi_core::types::MessageId::new().as_uuid()
                )
            });
        let relative = PathBuf::from("tool-results").join(format!("{leaf}.txt"));
        // Rooted atomic directory preparation cannot follow a planted parent
        // symlink. The actual report is created exclusively on its exact inode.
        self.fs
            .write_file_rooted_atomic(
                &self.session_dir,
                Path::new("tool-results/.handback-directory"),
                "",
            )
            .await
            .ok()?;
        let mut units: Vec<u16> = text.iter().copied().take(MAX_PERSIST_UTF16).collect();
        let truncated = (original_size > MAX_PERSIST_UTF16).then_some(MAX_PERSIST_UTF16);
        if truncated.is_some()
            && units
                .last()
                .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
        {
            units.pop();
        }
        let persisted = String::from_utf16_lossy(&units);
        match self
            .fs
            .write_new_file_rooted_no_follow(&self.session_dir, &relative, &persisted)
            .await
        {
            Ok(()) => {}
            Err(FsError::AlreadyExists(_)) => {
                // Native s6 accepts an immutable regular collision only when
                // its pinned inode has one link. This metadata check never
                // reads an old output merely to advertise its existing path.
                self.fs
                    .validate_file_rooted_single_link(&self.session_dir, &relative)
                    .await
                    .ok()?;
            }
            Err(_) => return None,
        }
        let has_more = units.len() > PREVIEW_UTF16;
        if has_more {
            units.truncate(PREVIEW_UTF16);
            if let Some(newline) = units.iter().rposition(|unit| *unit == u16::from(b'\n')) {
                if newline > PREVIEW_UTF16 / 2 {
                    units.truncate(newline);
                }
            }
        }
        let filepath = self.session_dir.join(relative);
        let lead = truncated.map_or_else(
            || {
                format!(
                    "Output too large ({}). Full output saved to: {}\n\n",
                    format_size(original_size),
                    filepath.display()
                )
            },
            |limit| {
                format!(
                    "Output exceeded the {} persist limit; only the first {} were saved to: {}\n\n",
                    format_size(limit),
                    format_size(limit),
                    filepath.display()
                )
            },
        );
        let mut pointer: Vec<u16> = format!(
            "<persisted-output>\n{lead}Preview (first {}):\n",
            format_size(PREVIEW_UTF16)
        )
        .encode_utf16()
        .collect();
        pointer.extend(units);
        pointer.extend(
            if has_more {
                "\n...\n</persisted-output>"
            } else {
                "\n</persisted-output>"
            }
            .encode_utf16(),
        );
        Some(PersistedHandbackBody {
            text: Utf16JsonProjection::root_string(String::from_utf16_lossy(&pointer), pointer)
                .expect("persisted Handback pointer is a source string"),
        })
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_size(size: usize) -> String {
    if size < 1_024 {
        return format!("{size} bytes");
    }
    let (value, suffix) = if size < 1_048_576 {
        (size as f64 / 1_024.0, "KB")
    } else if size < 1_073_741_824 {
        (size as f64 / 1_048_576.0, "MB")
    } else {
        (size as f64 / 1_073_741_824.0, "GB")
    };
    let formatted = format!("{:.1}", (value * 10.0).round() / 10.0);
    format!(
        "{}{suffix}",
        formatted.strip_suffix(".0").unwrap_or(&formatted)
    )
}
