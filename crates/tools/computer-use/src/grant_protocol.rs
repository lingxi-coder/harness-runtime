//! Latest computer MCP application approval and discovery routes.
use super::{
    app_policy, json,
    permission_model::{AppTier, GrantFlags},
    protocol_result, ComputerTool, JsonValueView, ToolCallResult, ToolError, ToolProgressSender,
    ToolUseContext, Utf16JsonProjection, Value,
};
use base64::Engine;
use lingxi_core::host::computer_control::AppInfo;
use once_cell::sync::Lazy;
use permission::computer_access::{ComputerAccessRequest, ComputerInteraction, RequestedApp};
use std::collections::HashSet;
use unicode_normalization::UnicodeNormalization;

static HELPERS: Lazy<Value> = Lazy::new(|| {
    serde_json::from_str(include_str!("../fixtures/grants-2.1.325.json"))
        .expect("pinned grant helper fixture")
});
static LABEL: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r#"^[\p{L}\p{N}_ .&'"()+\-–—→⌘⌥⌃⇧]+$"#).unwrap());
fn constant(name: &str) -> &'static str {
    HELPERS["constants"][name]
        .as_str()
        .expect("pinned native string")
}
fn poisoned() -> ToolError {
    ToolError::Internal("computer protocol state poisoned".into())
}
fn js_space(c: char) -> bool {
    c.is_whitespace() && c != '\u{85}' || c == '\u{feff}'
}
fn units(s: &str) -> usize {
    s.encode_utf16().count()
}
fn safe_label(s: &str) -> Option<String> {
    let mut text = s.trim_matches(js_space).to_owned();
    if text.is_empty() {
        return None;
    }
    for (a, b) in [
        ('‘', "'"),
        ('’', "'"),
        ('“', "\""),
        ('”', "\""),
        ('…', "..."),
    ] {
        text = text.replace(a, b);
    }
    (units(&text) <= 40 && LABEL.is_match(&text)).then_some(text)
}
fn safe_id(s: &str) -> Option<&str> {
    (!s.is_empty()
        && units(s) <= 1024
        && !s.chars().any(|c| {
            c <= '\u{1f}'
                || ('\u{7f}'..='\u{9f}').contains(&c)
                || matches!(c, '\u{2028}' | '\u{2029}')
                || ('\u{e0000}'..='\u{e007f}').contains(&c)
        })
        && !s.nfkd().any(|c| matches!(c, '<' | '>' | '"')))
    .then_some(s)
}
fn app_label(record: &Value) -> String {
    record["displayName"]
        .as_str()
        .and_then(safe_label)
        .or_else(|| record["bundleId"].as_str().and_then(safe_label))
        .unwrap_or_else(|| "(name withheld)".into())
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn record(app: &AppInfo, tier: AppTier, at: u64) -> Value {
    json!({"bundleId":app.bundle_id,"displayName":app.display_name,"grantedAt":at,"tier":tier.as_str()})
}
fn list_names(apps: &[Value], field: &str) -> String {
    apps.iter()
        .map(|a| format!("\"{}\"", a[field].as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join(", ")
}
fn denied_guidance(apps: &[Value], policy: bool) -> String {
    let names = list_names(apps, "displayName");
    let one = apps.len() == 1;
    if policy {
        format!("{names} {} blocked by policy for computer use. Requests for {} are automatically denied regardless of what the user has approved. There is no Settings override. Inform the user that you cannot access {} and suggest an alternative approach if one exists. Do not try to directly subvert this block regardless of the user's request.",if one{"is"}else{"are"},if one{"this app"}else{"these apps"},if one{"this app"}else{"these apps"})
    } else {
        format!("{names} {} in the user's auto-deny list (Settings → Desktop app (General) → Computer Use → Denied apps). Requests for {} are automatically denied. If you need access for this task, ask the user to remove {} from their deny list in Settings — you cannot request this through the tool.",if one{"is"}else{"are"},if one{"this app"}else{"these apps"},if one{"it"}else{"them"})
    }
}
fn not_installed_guidance(apps: &[Value], flags: bool) -> String {
    let names = list_names(apps, "requestedName");
    let one = apps.len() == 1;
    let suggested = apps
        .iter()
        .filter(|app| app["didYouMean"].as_array().is_some_and(|a| !a.is_empty()))
        .map(|app| {
            format!(
                "{} → {}",
                app["requestedName"].as_str().unwrap_or(""),
                app["didYouMean"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(" or ")
            )
        })
        .collect::<Vec<_>>();
    let hint = if suggested.is_empty() {
        String::new()
    } else {
        format!(" Did you mean: {}?", suggested.join("; "))
    };
    format!("{names} {} match any installed or running application. The request was NOT shown to the user.{hint} Retry request_access with the corrected name{} (include any other apps from this call too — the whole call was short-circuited{}). If you're unsure of the exact name, ask the user.",if one{"doesn't"}else{"don't"},if one{""}else{"s"},if flags{", as were the clipboard/systemKeyCombos flags you passed"}else{""})
}
fn distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a: Vec<_> = a.encode_utf16().collect();
    let b: Vec<_> = b.encode_utf16().collect();
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (i, ac) in a.iter().enumerate() {
        let mut next = vec![i + 1];
        let mut minimum = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let n = (row[j + 1] + 1)
                .min(next[j] + 1)
                .min(row[j] + usize::from(ac != bc));
            next.push(n);
            minimum = minimum.min(n);
        }
        if minimum > limit {
            return None;
        }
        row = next;
    }
    (row[b.len()] <= limit).then_some(row[b.len()])
}
fn suggestions(query: &str, apps: &[AppInfo]) -> Vec<String> {
    let query = query.to_lowercase();
    let q = query.trim_matches(js_space);
    if units(q) < 3 {
        return vec![];
    }
    let words = q
        .split(js_space)
        .filter(|w| units(w) >= 4)
        .collect::<Vec<_>>();
    let mut scored = vec![];
    for (order, app) in apps.iter().enumerate() {
        let name = app.display_name.to_lowercase();
        let diff = units(q).abs_diff(units(&name)).min(50) as i64;
        let score = if name.contains(q) {
            1000 - diff
        } else if units(&name) >= 4 && q.contains(&name) {
            900 - diff
        } else if units(q) >= 4 && app.bundle_id.to_lowercase().contains(q) {
            800
        } else if let Some(d) = distance(q, &name, 2.max(units(q).min(units(&name)) / 4)) {
            700 - d as i64 * 10 - diff
        } else if !words.is_empty() {
            let other = name
                .split(js_space)
                .filter(|w| units(w) >= 4)
                .collect::<Vec<_>>();
            if other.is_empty() {
                0
            } else {
                let near = |a: &str, b: &str| {
                    distance(a, b, if units(a).min(units(b)) <= 4 { 1 } else { 2 }).is_some()
                };
                let a = words
                    .iter()
                    .filter(|w| other.iter().any(|o| near(w, o)))
                    .count() as f64
                    / words.len() as f64;
                let b = other
                    .iter()
                    .filter(|w| words.iter().any(|o| near(w, o)))
                    .count() as f64
                    / other.len() as f64;
                let ratio = a.max(b);
                if ratio == 1.0 {
                    500 - diff
                } else if ratio >= 0.5 {
                    (300.0 * ratio).floor() as i64
                } else {
                    0
                }
            }
        } else {
            0
        };
        if score > 0 {
            scored.push((score, order, app));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut seen = HashSet::new();
    scored
        .into_iter()
        .filter(|(_, _, a)| seen.insert(a.bundle_id.clone()))
        .filter_map(|(_, _, a)| safe_label(&a.display_name))
        .take(3)
        .collect()
}
fn tier_guidance(apps: &[Value]) -> String {
    let mut groups = [vec![], vec![], vec![], vec![]];
    for app in apps {
        let cat = app_policy::category(
            app["bundleId"].as_str(),
            app["displayName"].as_str().unwrap_or(""),
        );
        match app["tier"].as_str() {
            Some("read") => groups[usize::from(cat != Some("browser"))].push(app),
            Some("click") => groups[if cat == Some("shell") { 3 } else { 2 }].push(app),
            _ => {}
        }
    }
    let mut output = vec![];
    for (i, group) in groups.iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        let names = group
            .iter()
            .map(|a| format!("\"{}\"", app_label(a)))
            .collect::<Vec<_>>()
            .join(", ");
        let one = group.len() == 1;
        output.push(match i {
    0=>format!("{names} {} — granted at tier \"read\" (visible in screenshots only; no clicks or typing). You can read what's on screen but cannot navigate, click, or type into {}. For browser interaction, use the Claude-in-Chrome MCP (tools named `mcp__claude-in-chrome__*`; load via ToolSearch if deferred).",if one{"is a browser"}else{"are browsers"},if one{"it"}else{"them"}),
    1=>format!("{names} {} granted at tier \"read\" (visible in screenshots only; no clicks or typing). You can read what's on screen but cannot interact. Ask the user to take any actions in {} themselves.",if one{"is"}else{"are"},if one{"this app"}else{"these apps"}),
    2=>format!("{names} {} terminal or IDE capabilities — granted at tier \"click\" (visible + plain left-click only; NO typing, key presses, right-click, modifier-clicks, or drag-drop). You can click buttons and scroll output, but {} integrated terminal and editor are off-limits to keyboard input. Right-click (context-menu Paste) and dragging text onto {} require tier \"full\". For shell commands, use the Bash tool.",if one{"has"}else{"have"},if one{"its"}else{"their"},if one{"it"}else{"them"}),
    _=>format!("{names} {} the Windows desktop shell — granted at tier \"click\" (visible + plain left-click only; NO typing, key presses, right-click, modifier-clicks, or drag-drop). You can click to open folders and items, but typing is blocked: the address bar, Search box, and Run dialog hand typed text to ShellExecute. For shell commands, use the Bash tool.",if one{"is"}else{"are"})});
    }
    if output.is_empty() {
        String::new()
    } else {
        output.join("\n\n") + constant("U")
    }
}

fn list_collator(locale: &str) -> icu_collator::CollatorBorrowed<'static> {
    use icu_collator::options::{AlternateHandling, CollatorOptions, Strength};
    use icu_collator::preferences::{CollationCaseFirst, CollationNumericOrdering};
    let locale = icu_locale::Locale::try_from_str(locale)
        .unwrap_or_else(|_| icu_locale::Locale::try_from_str("en-US").unwrap());
    let mut preferences = icu_collator::CollatorPreferences::from(&locale);
    preferences.case_first = Some(CollationCaseFirst::False);
    preferences.numeric_ordering = Some(CollationNumericOrdering::False);
    let mut options = CollatorOptions::default();
    options.strength = Some(Strength::Tertiary);
    options.alternate_handling = Some(AlternateHandling::NonIgnorable);
    icu_collator::Collator::try_new(preferences, options).expect("compiled ICU collation profile")
}
fn list_default_locale() -> String {
    #[cfg(any(target_vendor = "apple", target_os = "windows", target_os = "android"))]
    let locale = sys_locale::get_locale();
    #[cfg(all(unix, not(any(target_vendor = "apple", target_os = "android"))))]
    let locale = lingxi_core::native_locale::current_lc_ctype();
    #[cfg(not(any(unix, target_os = "windows", target_os = "android")))]
    let locale: Option<String> = None;
    let locale = locale.as_deref().unwrap_or("en-US");
    let locale = locale
        .split('.')
        .next()
        .unwrap_or(locale)
        .split('@')
        .next()
        .unwrap_or(locale);
    if locale.is_empty() || locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX")
    {
        "en-US".into()
    } else {
        locale.replace('_', "-")
    }
}
fn cursor_offset(value: Option<&str>) -> usize {
    let Some(value) = value else {
        return 0;
    };
    // Node Buffer.from(base64) accepts omitted padding, URL alphabet and noise.
    let mut encoded = value
        .chars()
        .filter_map(|c| match c {
            '-' => Some('+'),
            '_' => Some('/'),
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=') => Some(c),
            _ => None,
        })
        .collect::<String>();
    if let Some(end) = encoded.find('=') {
        encoded.truncate(end);
    }
    if encoded.len() % 4 == 1 {
        encoded.pop();
    }
    while encoded.len() % 4 != 0 {
        encoded.push('=');
    }
    let Some(bytes) = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
    else {
        return 0;
    };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches(js_space);
    let text = text.strip_prefix('+').unwrap_or(text);
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}
fn list_apps_output(
    mut records: Vec<Value>,
    input: &Value,
    denied: &HashSet<String>,
    host_id: &str,
    locale: &str,
) -> Value {
    let query = input["query"].as_str().map(str::to_lowercase);
    let running_first = input["running_first"] != false;
    records.retain(|a| {
        let id = a["bundleId"].as_str().unwrap_or("");
        let name = a["displayName"].as_str().unwrap_or("");
        !denied.contains(id)
            && !app_policy::blocked(Some(id), name)
            && id != host_id
            && query
                .as_ref()
                .is_none_or(|q| id.to_lowercase().contains(q) || name.to_lowercase().contains(q))
    });
    let collator = list_collator(locale);
    records.sort_by(|a, b| {
        let run = if running_first {
            b["isRunning"]
                .as_bool()
                .unwrap_or(false)
                .cmp(&a["isRunning"].as_bool().unwrap_or(false))
        } else {
            std::cmp::Ordering::Equal
        };
        run.then_with(|| {
            collator.compare(
                a["bundleId"].as_str().unwrap_or(""),
                b["bundleId"].as_str().unwrap_or(""),
            )
        })
    });
    let limit = input["limit"]
        .as_f64()
        .filter(|n| *n >= 1.0)
        .map_or(25, |n| n.min(200.0).trunc() as usize);
    let cursor = cursor_offset(input["cursor"].as_str());
    let apps=records.iter().skip(cursor).take(limit).map(|app|{
        let mut output=json!({"bundleId":safe_id(app["bundleId"].as_str().unwrap_or("")).unwrap_or("(id withheld)"),"displayName":safe_label(app["displayName"].as_str().unwrap_or("")).unwrap_or_else(||"(name withheld)".into()),"isRunning":app["isRunning"].as_bool().unwrap_or(false)});
        if app["isRunning"]==true&&app["isFromDevPath"]==true{output["isRunningFromDevPath"]=true.into();}
        if let Some(pid)=app.get("pid"){output["pid"]=pid.clone();}
        output
    }).collect::<Vec<_>>();
    let mut output = json!({"apps":apps});
    if cursor.saturating_add(limit) < records.len() {
        output["nextCursor"] = Value::String(
            base64::engine::general_purpose::STANDARD
                .encode(cursor.saturating_add(limit).to_string()),
        );
    }
    output
}
fn window_locations(
    granted: &[Value],
    displays: &[lingxi_core::host::computer_control::DisplayInfo],
    locations: &Value,
) -> Vec<Value> {
    if displays.len() <= 1 || granted.is_empty() {
        return vec![];
    }
    let names = super::display_protocol::display_names(displays);
    let empty = Vec::new();
    let locations = locations.as_array().unwrap_or(&empty);
    let mut output = Vec::new();
    for app in granted {
        let Some(ids) = locations
            .iter()
            .find(|location| location["bundleId"] == app["bundleId"])
            .and_then(|location| location["displayIds"].as_array())
            .filter(|ids| !ids.is_empty())
        else {
            continue;
        };
        let ids = ids
            .iter()
            .map(|id| {
                let mut result = json!({"id":id});
                if let Some(id) = id.as_u64().and_then(|id| u32::try_from(id).ok()) {
                    if let Some(name) = names.get(&id) {
                        result["label"] = name.clone().into();
                    }
                    if let Some(display) = displays.iter().find(|display| display.id == id) {
                        result["isPrimary"] = display.is_primary.into();
                    }
                }
                result
            })
            .collect::<Vec<_>>();
        output.push(
            json!({"bundleId":app["bundleId"],"displayName":app["displayName"],"displays":ids}),
        );
    }
    output
}
fn exact(value: JsonValueView<'_>) -> Option<String> {
    String::from_utf16(value.code_units()?).ok()
}
fn input_value(input: &Utf16JsonProjection) -> Value {
    serde_json::from_str(&input.to_json_string().unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|_| json!({}))
}
fn flags_value(flags: GrantFlags) -> Value {
    json!({"clipboardRead":flags.clipboard_read,"clipboardWrite":flags.clipboard_write,"systemKeyCombos":flags.system_key_combos})
}
fn add_denials(output: &mut Value, policy: &[Value], user: &[Value]) {
    if !policy.is_empty() {
        output["policyDenied"] = json!({"apps":policy,"guidance":denied_guidance(policy,true)});
    }
    if !user.is_empty() {
        output["userDenied"] = json!({"apps":user,"guidance":denied_guidance(user,false)});
    }
}

impl ComputerTool {
    pub(super) async fn call_grant_protocol(
        &self,
        action: &str,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let revision = self.sync_policy(&ctx).await?;
        self.check_cancel(&ctx)?;
        if action == "request_access" && !self.access_resolver.supports_app_approval() {
            return Ok(protocol_result::error("This session was not wired with a permission handler. Computer control is not available here.","feature_unavailable"));
        }
        if action == "request_teach_access" && !self.access_resolver.supports_interactions() {
            return Ok(protocol_result::error(
                "Teach mode is not available in this session.",
                "feature_unavailable",
            ));
        }
        if action == "list_granted_applications" {
            let apps = self.protocol_records()?;
            let flags = self.state.lock().map_err(|_| poisoned())?.grant_flags;
            return Ok(protocol_result::json(
                json!({"allowedApps":apps,"grantFlags":flags_value(flags)}),
            ));
        }
        let backend = self
            .ctx
            .computer_control
            .as_ref()
            .ok_or_else(|| ToolError::Internal("computer control unavailable".into()))?;
        let values = input_value(&input);
        if action == "list_apps" {
            if !backend.supports_app_scoped() {
                return Ok(protocol_result::error(
                    "list_apps is not available on this platform.",
                    "feature_unavailable",
                ));
            }
            let native_input = tool_api::tool_trait::generated_builtin_result(&json!({}));
            let native = backend.app_scoped("list_apps", "", &native_input).await;
            self.check_cancel(&ctx)?;
            self.check_policy_revision(revision.as_deref())?;
            let mut records = match native {
                Ok(native) => serde_json::from_str::<Value>(
                    &native
                        .to_json_string()
                        .map_err(|e| ToolError::Internal(e.to_string()))?,
                )
                .map_err(|e| ToolError::Internal(e.to_string()))?
                .as_array()
                .cloned()
                .ok_or_else(|| {
                    ToolError::Internal("invalid native application catalogue".into())
                })?,
                Err(lingxi_core::host::computer_control::ComputerError::Unsupported(_)) => {
                    let installed = backend
                        .list_installed_apps()
                        .await
                        .map_err(|e| super::map_err(&e))?;
                    let running = backend.list_running_apps().await.unwrap_or_default();
                    self.check_cancel(&ctx)?;
                    self.check_policy_revision(revision.as_deref())?;
                    let running_ids = running
                        .iter()
                        .map(|a| a.bundle_id.as_str())
                        .collect::<HashSet<_>>();
                    let mut all = installed.clone();
                    let mut seen = all
                        .iter()
                        .map(|a| a.bundle_id.clone())
                        .collect::<HashSet<_>>();
                    for app in &running {
                        if seen.insert(app.bundle_id.clone()) {
                            all.push(app.clone());
                        }
                    }
                    all.iter().map(|a|json!({"bundleId":a.bundle_id,"displayName":a.display_name,"isRunning":running_ids.contains(a.bundle_id.as_str())})).collect()
                }
                Err(error) => return Err(super::map_err(&error)),
            };
            let policy = self.policy_snapshot()?;
            let denied = policy
                .as_ref()
                .map(|(_, policy)| {
                    policy
                        .apps
                        .iter()
                        .filter(|a| a.permission == "deny")
                        .map(|a| a.bundle_id.clone())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            records.retain(|a| !self.is_own_app(a["bundleId"].as_str().unwrap_or("")));
            let output = list_apps_output(
                records,
                &values,
                &denied,
                branding::MANAGED_PREFERENCES_DOMAIN,
                &list_default_locale(),
            );
            return Ok(protocol_result::json(output));
        }
        let teach = action == "request_teach_access";
        let active = self.protocol.lock().map_err(|_| poisoned())?.teach_active;
        if active {
            return Ok(protocol_result::error(
                if teach {
                    "Teach mode is already active. To add more apps, end the current tour first, then call request_teach_access again with the full app list."
                } else {
                    "Cannot request additional permissions during teach mode — the permission dialog would be hidden. End teach mode (finish the tour or let the turn complete), then call request_access, then start a new tour."
                },
                "teach_mode_conflict",
            ));
        }
        let Some(reason) = input.get_property("reason").and_then(exact) else {
            return Ok(protocol_result::error(
                "\"reason\" must be a string.",
                "bad_args",
            ));
        };
        if let Some((accessibility, screen_recording)) = self.read_os_permissions(&ctx).await? {
            if !accessibility || !screen_recording {
                if ctx.is_scheduled_turn {
                    return Ok(protocol_result::error(&format!("macOS Accessibility / Screen Recording permissions aren't granted, and the grant prompt can't be shown during a scheduled run. Grant them in the {} desktop app, then re-run the task. (Retrying returns this same result.)", branding::PRODUCT_NAME),"unattended_no_approver"));
                }
                self.revoke_os_permissions(&ctx).await?;
                // The explicit interactive setup request registers this application
                // with TCC. Read-only queries and ordinary effects never prompt.
                let _ = backend.check_os_permissions().await;
                self.check_cancel(&ctx)?;
                self.check_policy_revision(revision.as_deref())?;
                let request = ComputerAccessRequest {
                    reason,
                    apps: vec![],
                    tier: AppTier::Full.to_bridge(),
                    clipboard_read: false,
                    clipboard_write: false,
                    system_key_combos: false,
                    tcc_state: Some(permission::computer_access::TccState {
                        accessibility,
                        screen_recording,
                    }),
                    interaction: None,
                };
                let _ = self.resolve_protocol_interaction(request, &ctx).await?;
                let permissions = self
                    .read_os_permissions(&ctx)
                    .await?
                    .unwrap_or((false, false));
                self.check_cancel(&ctx)?;
                self.check_policy_revision(revision.as_deref())?;
                let result = super::permission_result::tcc_setup_result(permissions);
                let mut wire =
                    serde_json::from_str::<Value>(&result.data.to_json_string().unwrap()).unwrap();
                if teach {
                    if let Some(s) = wire["content"][0]["text"].as_str() {
                        wire["content"][0]["text"] =
                            Value::String(s.replace("request_access", "request_teach_access"));
                    }
                }
                return Ok(protocol_result::from_wire(wire));
            }
        }
        let Some(app_values) = input.get_property("apps") else {
            return Ok(protocol_result::error(
                "\"apps\" must be an array of strings.",
                "bad_args",
            ));
        };
        let Some(len) = app_values.array_len() else {
            return Ok(protocol_result::error(
                "\"apps\" must be an array of strings.",
                "bad_args",
            ));
        };
        let Some(requested) = (0..len)
            .map(|i| app_values.get_index(i).and_then(exact))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(protocol_result::error(
                "\"apps\" must be an array of strings.",
                "bad_args",
            ));
        };
        // Native Fo/qo reach application enumeration only after their
        // handler, teach-conflict, argument and system-permission guards.
        let installed = backend
            .list_installed_apps()
            .await
            .map_err(|e| super::map_err(&e))?;
        self.check_cancel(&ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        let running = backend.list_running_apps().await.unwrap_or_default();
        self.check_cancel(&ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        let mut system_app_ids = HashSet::new();
        if backend.supports_app_scoped() {
            let native_input = tool_api::tool_trait::generated_builtin_result(&json!({}));
            if let Ok(catalogue) = backend.app_scoped("list_apps", "", &native_input).await {
                if let Ok(catalogue) = serde_json::from_str::<Value>(
                    &catalogue.to_json_string().unwrap_or_else(|_| "[]".into()),
                ) {
                    if let Some(apps) = catalogue.as_array() {
                        for app in apps {
                            if app["path"]
                                .as_str()
                                .is_some_and(|path| path.starts_with("/System/Applications/"))
                            {
                                if let Some(id) = app["bundleId"].as_str() {
                                    system_app_ids.insert(id.to_owned());
                                }
                            }
                        }
                    }
                }
            }
            self.check_cancel(&ctx)?;
            self.check_policy_revision(revision.as_deref())?;
        }
        let policy = self.policy_snapshot()?;
        let at = now_ms();
        let previous = self.protocol_records()?;
        let previous_ids = previous
            .iter()
            .filter_map(|v| v["bundleId"].as_str())
            .collect::<HashSet<_>>();
        let flags = self.state.lock().map_err(|_| poisoned())?.grant_flags;
        let requested_flags = GrantFlags {
            clipboard_read: !teach && values["clipboardRead"] == true && !flags.clipboard_read,
            clipboard_write: !teach && values["clipboardWrite"] == true && !flags.clipboard_write,
            system_key_combos: !teach
                && values["systemKeyCombos"] == true
                && !flags.system_key_combos,
        };
        let mut candidates = vec![];
        let mut skip = vec![];
        let mut policy_denied = vec![];
        let mut user_denied = vec![];
        let mut missing = vec![];
        for name in requested {
            let resolved = if cfg!(target_os = "macos") && name.to_lowercase() == "finder" {
                Some(AppInfo {
                    bundle_id: "com.apple.finder".into(),
                    display_name: "Finder".into(),
                })
            } else {
                installed
                    .iter()
                    .find(|a| a.bundle_id == name)
                    .or_else(|| {
                        let mut selected = None;
                        for app in installed
                            .iter()
                            .filter(|a| a.display_name.to_lowercase() == name.to_lowercase())
                        {
                            if selected.is_none()
                                || system_app_ids.contains(&app.bundle_id)
                                || !selected.is_some_and(|old: &AppInfo| {
                                    system_app_ids.contains(&old.bundle_id)
                                })
                            {
                                selected = Some(app);
                            }
                        }
                        selected
                    })
                    .cloned()
                    .or_else(|| {
                        let path = name.to_lowercase().replace('/', "\\");
                        let tail = path.rsplit('\\').next().unwrap_or(&path);
                        running
                            .iter()
                            .find(|a| {
                                a.bundle_id.to_lowercase().replace('/', "\\") == path
                                    || a.bundle_id.to_lowercase().rsplit(['\\', '/']).next()
                                        == Some(tail)
                            })
                            .cloned()
                    })
            };
            let id = resolved
                .as_ref()
                .map_or(name.as_str(), |a| a.bundle_id.as_str());
            let display = resolved
                .as_ref()
                .map_or(name.as_str(), |a| a.display_name.as_str());
            if self.is_own_app(id) {
                return Ok(protocol_result::error(
                    &constant("Xt").replace("Claude", branding::PRODUCT_NAME),
                    "self_app_denied",
                ));
            }
            if app_policy::blocked(resolved.as_ref().map(|a| a.bundle_id.as_str()), display) {
                policy_denied.push(json!({"requestedName":name,"displayName":display}));
                continue;
            }
            let Some(app) = resolved else {
                missing
                    .push(json!({"requestedName":name,"didYouMean":suggestions(&name,&installed)}));
                continue;
            };
            if policy.as_ref().is_some_and(|(_, p)| {
                p.apps
                    .iter()
                    .any(|a| a.bundle_id == app.bundle_id && a.permission == "deny")
            }) {
                user_denied.push(json!({"requestedName":name,"displayName":app.display_name}));
                continue;
            }
            let tier = app_policy::maximum_tier(Some(&app.bundle_id), &app.display_name);
            if previous_ids.contains(app.bundle_id.as_str()) {
                skip.push(
                    previous
                        .iter()
                        .find(|v| v["bundleId"] == app.bundle_id)
                        .cloned()
                        .unwrap_or_else(|| record(&app, tier, at)),
                );
            } else {
                candidates.push((name, app, tier));
            }
        }
        if !missing.is_empty() {
            let mut output = json!({"granted":skip,"denied":[],"notInstalled":{"apps":missing,"guidance":not_installed_guidance(&missing,requested_flags.clipboard_read||requested_flags.clipboard_write||requested_flags.system_key_combos)}});
            add_denials(&mut output, &policy_denied, &user_denied);
            if teach {
                output["teachModeActive"] = false.into();
            }
            output["screenshotFiltering"] = "mask".into();
            return Ok(protocol_result::json(output));
        }
        if !teach {
            let turn = ctx
                .messages
                .iter()
                .rev()
                .find_map(|message| {
                    if let lingxi_core::types::ConversationMessage::User {
                        id,
                        content,
                        is_meta,
                        is_compact_summary,
                        ..
                    } = message
                    {
                        if !*is_meta
                            && !*is_compact_summary
                            && content.iter().any(|block| {
                                matches!(
                                    block,
                                    lingxi_core::types::ContentBlock::Text { .. }
                                        | lingxi_core::types::ContentBlock::TextJsUtf16 { .. }
                                        | lingxi_core::types::ContentBlock::Image { .. }
                                )
                            })
                        {
                            return Some(id.to_string());
                        }
                    }
                    None
                })
                .or_else(|| ctx.assistant_message_id.as_ref().map(ToString::to_string));
            let mut protocol = self.protocol.lock().map_err(|_| poisoned())?;
            if protocol.warn_turn != turn {
                protocol.warned.clear();
                protocol.warn_turn = turn;
            }
            let mut warnings = vec![];
            for (category, key) in [("browser", "Lo"), ("terminal", "Oo")] {
                if candidates.iter().any(|(_, app, _)| {
                    app_policy::category(Some(&app.bundle_id), &app.display_name) == Some(category)
                }) && protocol.warned.insert(category.into())
                {
                    warnings.push(constant(key));
                }
            }
            if !warnings.is_empty() {
                return Ok(protocol_result::error(
                    &warnings.join("\n\n"),
                    "restricted_app_first_request",
                ));
            }
        }
        if ctx.is_scheduled_turn
            && (!candidates.is_empty()
                || requested_flags.clipboard_read
                || requested_flags.clipboard_write
                || requested_flags.system_key_combos)
        {
            let flags = [
                ("clipboardRead", requested_flags.clipboard_read),
                ("clipboardWrite", requested_flags.clipboard_write),
                ("systemKeyCombos", requested_flags.system_key_combos),
            ]
            .into_iter()
            .filter_map(|(key, on)| on.then_some(key))
            .collect::<Vec<_>>()
            .join(", ");
            let names = if candidates.is_empty() {
                format!("grant flags ({flags})")
            } else {
                let names = candidates
                    .iter()
                    .map(|(name, _, _)| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                names
                    + &if flags.is_empty() {
                        String::new()
                    } else {
                        format!(" (and grant flags: {flags})")
                    }
            };
            let existing = if skip.is_empty() {
                String::new()
            } else {
                format!(
                    " Already-granted apps remain available: {}.",
                    skip.iter()
                        .filter_map(|a| a["displayName"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Ok(protocol_result::error(&format!("Computer-use access to {names} can't be approved during a scheduled run. To grant it, send a message in this conversation (the approval card will appear), or add {} to the scheduled task's settings. (Retrying returns this same result.){existing}",if candidates.is_empty(){"the flag"}else{"the app"}),"unattended_no_approver"));
        }
        if teach && candidates.is_empty() && skip.is_empty() {
            let mut output = json!({"granted":[],"denied":[]});
            add_denials(&mut output, &policy_denied, &user_denied);
            output["teachModeActive"] = false.into();
            output["screenshotFiltering"] = "mask".into();
            return Ok(protocol_result::json(output));
        }
        let needs_dialog = teach
            || !candidates.is_empty()
            || requested_flags.clipboard_read
            || requested_flags.clipboard_write
            || requested_flags.system_key_combos;
        let response = if needs_dialog {
            let tier = candidates
                .iter()
                .map(|(_, _, tier)| *tier)
                .max()
                .unwrap_or(AppTier::Read);
            self.resolve_protocol_interaction(
                ComputerAccessRequest {
                    reason: reason.clone(),
                    apps: candidates
                        .iter()
                        .map(|(_, app, _)| RequestedApp {
                            label: app.bundle_id.clone(),
                        })
                        .collect(),
                    tier: tier.to_bridge(),
                    clipboard_read: requested_flags.clipboard_read,
                    clipboard_write: requested_flags.clipboard_write,
                    system_key_combos: requested_flags.system_key_combos,
                    tcc_state: None,
                    interaction: teach.then(|| ComputerInteraction::TeachAccess {
                        reason: Some(reason),
                        explanation: None,
                        next_preview: None,
                        anchor: None,
                    }),
                },
                &ctx,
            )
            .await?
        } else {
            Default::default()
        };
        self.check_cancel(&ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        self.check_current_os_permissions(&ctx).await?;
        if teach && response.interaction_action.as_deref() != Some("allow") {
            return Ok(protocol_result::error("The user declined to start the guided walkthrough (teach mode). Do not call request_teach_access again for this same request. Ask the user how they would like to proceed — for example, whether you should just do the task directly instead of guiding them through it.","teach_declined"));
        }
        let mut newly = vec![];
        let mut denied = vec![];
        {
            let mut state = self.state.lock().map_err(|_| poisoned())?;
            let mut protocol = self.protocol.lock().map_err(|_| poisoned())?;
            if let Some(mode) = response.preferred_mode.as_ref() {
                if matches!(mode.as_str(), "background" | "full_control") {
                    protocol.preferred_mode = Some(mode.clone());
                }
            }
            for (name, app, tier) in candidates {
                if response.granted_apps.contains(&app.bundle_id) {
                    let value = protocol
                        .app_records
                        .entry(app.bundle_id.clone())
                        .or_insert_with(|| record(&app, tier, at))
                        .clone();
                    state.grant_app(app.bundle_id, tier);
                    newly.push(value);
                } else {
                    denied.push(Value::String(name));
                }
            }
            state.merge_grant_flags(GrantFlags {
                clipboard_read: response.clipboard_read && requested_flags.clipboard_read,
                clipboard_write: response.clipboard_write && requested_flags.clipboard_write,
                system_key_combos: response.system_key_combos && requested_flags.system_key_combos,
            });
            if teach {
                protocol.teach_active = false;
            }
        }
        if !newly.is_empty() || !skip.is_empty() {
            self.os_permissions_revoked
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        skip.extend(newly);
        if teach && !skip.is_empty() {
            if tool_api::tool_trait::Tool::begin_computer_sequence(self, &ctx)
                .await
                .is_err()
            {
                return Ok(protocol_result::error("Another Claude session started using the computer while this teach request was awaiting approval, so teach mode could not start. Ask the user to try again once the other session finishes.", "cu_lock_held"));
            }
            self.check_cancel(&ctx)?;
            self.check_policy_revision(revision.as_deref())?;
            self.ensure_escape_registration(&ctx, &tx).await?;
            self.protocol.lock().map_err(|_| poisoned())?.teach_active = true;
        }
        let guidance = tier_guidance(&skip);
        let mut output = json!({"granted":skip,"denied":denied});
        add_denials(&mut output, &policy_denied, &user_denied);
        if !guidance.is_empty() {
            output["tierGuidance"] = guidance.into();
        }
        if teach {
            output["teachModeActive"] = self
                .protocol
                .lock()
                .map_err(|_| poisoned())?
                .teach_active
                .into();
        }
        output["screenshotFiltering"] = "mask".into();
        if !teach {
            let preferred = self
                .protocol
                .lock()
                .map_err(|_| poisoned())?
                .preferred_mode
                .clone();
            if let Some(mode) = preferred {
                output["preferredModeGuidance"] = Value::String(if mode == "background" {
                    "The user prefers BACKGROUND control. Use the app_* tools (app_screenshot, app_click, app_type, etc.) so the user can keep working in other apps while you act on the granted ones. Only fall back to the full-screen tools (screenshot, left_click, etc.) when an app_* call returns 'unsupported' and there is no other path — and expect a separate approval dialog when you do.".into()
                } else {
                    "The user prefers FULL-SCREEN control. Use screenshot, left_click, type, etc. (which take over the screen with the glow border). The app_* background tools are still available if you only need to read or make small edits without interrupting the user.".into()
                });
            }
        }
        if !teach {
            if let Ok(displays) = backend.list_displays().await {
                if displays.len() > 1 {
                    let requested = output["granted"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|a| a["bundleId"].as_str())
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    let native_input = tool_api::tool_trait::generated_builtin_result(
                        &json!({"bundleIds":requested}),
                    );
                    if let Ok(locations) = backend
                        .app_scoped("find_window_displays", "", &native_input)
                        .await
                    {
                        if let Ok(locations) = serde_json::from_str::<Value>(
                            &locations.to_json_string().unwrap_or_else(|_| "[]".into()),
                        ) {
                            let locations = window_locations(
                                output["granted"].as_array().unwrap(),
                                &displays,
                                &locations,
                            );
                            if !locations.is_empty() {
                                output["windowLocations"] = Value::Array(locations);
                            }
                        }
                    }
                }
            }
            self.check_cancel(&ctx)?;
            self.check_policy_revision(revision.as_deref())?;
        }
        Ok(protocol_result::json(output))
    }
    fn is_own_app(&self, id: &str) -> bool {
        id == branding::MANAGED_PREFERENCES_DOMAIN
            || id
                .strip_prefix(branding::MANAGED_PREFERENCES_DOMAIN)
                .is_some_and(|tail| tail.starts_with('.'))
    }
    fn protocol_records(&self) -> Result<Vec<Value>, ToolError> {
        let names = self
            .policy_snapshot()?
            .map(|(_, policy)| {
                policy
                    .apps
                    .into_iter()
                    .map(|app| (app.bundle_id, app.display_name))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .unwrap_or_default();
        let state = self.state.lock().map_err(|_| poisoned())?;
        let mut protocol = self.protocol.lock().map_err(|_| poisoned())?;
        let at = now_ms();
        let mut records = vec![];
        for app in &state.allowed_apps {
            let value=protocol.app_records.entry(app.bundle_id.clone()).or_insert_with(||json!({"bundleId":app.bundle_id,"displayName":names.get(&app.bundle_id).unwrap_or(&app.bundle_id),"grantedAt":at,"tier":app.tier.as_str()}));
            value["tier"] = app.tier.as_str().into();
            records.push(value.clone());
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct CatalogueBackend;
    #[async_trait::async_trait]
    impl lingxi_core::host::computer_control::ComputerControl for CatalogueBackend {
        async fn screenshot(
            &self,
        ) -> Result<
            lingxi_core::host::computer_control::Screenshot,
            lingxi_core::host::computer_control::ComputerError,
        > {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no capture effects".into(),
                ),
            )
        }
        async fn display_size(
            &self,
        ) -> Result<(u32, u32), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no display effects".into(),
                ),
            )
        }
        async fn mouse_move(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn left_click(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn right_click(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn double_click(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn type_text(
            &self,
            _text: String,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn key(
            &self,
            _key: String,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn scroll(
            &self,
            _x: u32,
            _y: u32,
            _dx: i32,
            _dy: i32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }

        async fn list_installed_apps(
            &self,
        ) -> Result<Vec<AppInfo>, lingxi_core::host::computer_control::ComputerError> {
            Ok(vec![
                AppInfo {
                    bundle_id: "org.example.notes".into(),
                    display_name: "Notes".into(),
                },
                AppInfo {
                    bundle_id: "com.google.Chrome".into(),
                    display_name: "Chrome".into(),
                },
                AppInfo {
                    bundle_id: branding::MANAGED_PREFERENCES_DOMAIN.into(),
                    display_name: branding::PRODUCT_NAME.into(),
                },
            ])
        }
        async fn list_running_apps(
            &self,
        ) -> Result<Vec<AppInfo>, lingxi_core::host::computer_control::ComputerError> {
            Ok(vec![])
        }
        async fn query_os_permissions(
            &self,
        ) -> Result<Option<(bool, bool)>, lingxi_core::host::computer_control::ComputerError>
        {
            Ok(Some((true, true)))
        }
    }
    fn catalogue_tool(
        resolver: std::sync::Arc<dyn super::super::ComputerAccessResolver>,
    ) -> ComputerTool {
        let mut context = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![],
        );
        context.computer_control = Some(std::sync::Arc::new(CatalogueBackend));
        ComputerTool::with_access_resolver(context, resolver)
    }
    fn model_json(result: &ToolCallResult) -> Value {
        serde_json::from_str(super::super::test_model_text(result.model_content.as_ref()).unwrap())
            .unwrap()
    }
    #[tokio::test]
    async fn unresolved_and_self_requests_cannot_open_the_approval_panel_or_grant_flags() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let tool = catalogue_tool(std::sync::Arc::new(super::super::TuiBridgeResolver::new(
            sender,
        )));
        let context = super::super::execution_tests::ctx();
        let unknown = tool
            .call_grant_protocol(
                "request_access",
                super::super::test_projection(
                    &json!({"reason":"inspect","apps":["Uninstalled"],"clipboardRead":true}),
                ),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let data = model_json(&unknown);
        assert_eq!(data["granted"], json!([]));
        assert_eq!(data["denied"], json!([]));
        assert_eq!(
            data["notInstalled"]["apps"][0]["requestedName"],
            "Uninstalled"
        );
        assert!(!tool.state.lock().unwrap().grant_flags.clipboard_read);
        let own = tool
            .call_grant_protocol(
                "request_access",
                super::super::test_projection(
                    &json!({"reason":"inspect","apps":[branding::PRODUCT_NAME]}),
                ),
                context,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(own.is_error);
        assert!(receiver.try_recv().is_err());
    }
    #[tokio::test]
    async fn granted_record_order_and_timestamp_survive_repeated_request() {
        let tool = catalogue_tool(std::sync::Arc::new(super::super::AutoGrantResolver));
        let context = super::super::execution_tests::ctx();
        let input = super::super::test_projection(
            &json!({"reason":"inspect","apps":["Notes"],"clipboardRead":true}),
        );
        let first = tool
            .call_grant_protocol(
                "request_access",
                input.clone(),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let second = tool
            .call_grant_protocol(
                "request_access",
                input,
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            model_json(&first)["granted"],
            model_json(&second)["granted"]
        );
        let listed = tool
            .call_grant_protocol(
                "list_granted_applications",
                super::super::test_projection(&json!({})),
                context,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let listed = model_json(&listed);
        assert_eq!(listed["allowedApps"], model_json(&first)["granted"]);
        assert_eq!(listed["grantFlags"]["clipboardRead"], true);
    }
    #[tokio::test]
    async fn restricted_app_requires_second_request_in_same_user_turn() {
        let tool = catalogue_tool(std::sync::Arc::new(super::super::AutoGrantResolver));
        let context = super::super::execution_tests::ctx();
        let input =
            super::super::test_projection(&json!({"reason":"see user browser","apps":["Chrome"]}));
        let warning = tool
            .call_grant_protocol(
                "request_access",
                input.clone(),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(warning.is_error);
        assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
        let allowed = tool
            .call_grant_protocol(
                "request_access",
                input,
                context,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(!allowed.is_error);
        assert_eq!(model_json(&allowed)["granted"][0]["tier"], "read");
    }
    #[tokio::test]
    async fn unavailable_approval_handler_uses_native_error_without_a_grant() {
        let tool = catalogue_tool(std::sync::Arc::new(super::super::DenyAllResolver));
        let result = tool
            .call_grant_protocol(
                "request_access",
                super::super::test_projection(&json!({"reason":"inspect","apps":["Notes"]})),
                super::super::execution_tests::ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(super::super::test_model_text(result.model_content.as_ref()),Some("This session was not wired with a permission handler. Computer control is not available here."));
        assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    }
    struct RegistrationBackend(std::sync::atomic::AtomicU64);
    #[async_trait::async_trait]
    impl lingxi_core::host::computer_control::ComputerControl for RegistrationBackend {
        async fn screenshot(
            &self,
        ) -> Result<
            lingxi_core::host::computer_control::Screenshot,
            lingxi_core::host::computer_control::ComputerError,
        > {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no capture effects".into(),
                ),
            )
        }
        async fn display_size(
            &self,
        ) -> Result<(u32, u32), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no display effects".into(),
                ),
            )
        }
        async fn mouse_move(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn left_click(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn right_click(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn double_click(
            &self,
            _x: u32,
            _y: u32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn type_text(
            &self,
            _text: String,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn key(
            &self,
            _key: String,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }
        async fn scroll(
            &self,
            _x: u32,
            _y: u32,
            _dx: i32,
            _dy: i32,
        ) -> Result<(), lingxi_core::host::computer_control::ComputerError> {
            Err(
                lingxi_core::host::computer_control::ComputerError::Unsupported(
                    "fixture has no input effects".into(),
                ),
            )
        }

        async fn list_installed_apps(
            &self,
        ) -> Result<Vec<AppInfo>, lingxi_core::host::computer_control::ComputerError> {
            Err(lingxi_core::host::computer_control::ComputerError::Other(
                "application enumeration is unavailable".into(),
            ))
        }
        async fn list_running_apps(
            &self,
        ) -> Result<Vec<AppInfo>, lingxi_core::host::computer_control::ComputerError> {
            Ok(vec![])
        }
        async fn query_os_permissions(
            &self,
        ) -> Result<Option<(bool, bool)>, lingxi_core::host::computer_control::ComputerError>
        {
            Ok(Some((false, true)))
        }
        async fn check_os_permissions(&self) -> Option<(bool, bool)> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some((false, true))
        }
    }
    #[tokio::test]
    async fn tcc_setup_opens_despite_failed_enumeration_and_only_explicit_setup_registers() {
        let backend = std::sync::Arc::new(RegistrationBackend(Default::default()));
        let mut context = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![],
        );
        context.computer_control = Some(backend.clone());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let tool = ComputerTool::with_access_resolver(
            context,
            std::sync::Arc::new(super::super::TuiBridgeResolver::new(sender)),
        );
        let context = super::super::execution_tests::ctx();
        assert_eq!(
            tool.read_os_permissions(&context).await.unwrap(),
            Some((false, true))
        );
        assert_eq!(backend.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let request = tool.call_grant_protocol(
            "request_access",
            super::super::test_projection(
                &json!({"reason":"set up native access","apps":["Notes"]}),
            ),
            context.clone(),
            tool_api::test_support::fresh_tx(),
        );
        let respond = async {
            let exchange = receiver.recv().await.unwrap();
            assert!(exchange.request.tcc_state.is_some());
            assert!(exchange.request.apps.is_empty());
            assert_eq!(backend.0.load(std::sync::atomic::Ordering::SeqCst), 1);
            exchange.resp_tx.send(Default::default()).unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(request, respond)
        })
        .await
        .unwrap();
        assert!(result.unwrap().is_error);
        assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
        assert_eq!(
            tool.read_os_permissions(&context).await.unwrap(),
            Some((false, true))
        );
        assert_eq!(backend.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn native_325_permission_helpers_match_source_replay() {
        for case in HELPERS["windowLocations"].as_array().unwrap() {
            let displays = case["displays"]
                .as_array()
                .unwrap()
                .iter()
                .map(|display| lingxi_core::host::computer_control::DisplayInfo {
                    id: display["displayId"].as_u64().unwrap() as u32,
                    name: display["label"].as_str().unwrap().into(),
                    width: 0,
                    height: 0,
                    is_primary: display["isPrimary"].as_bool().unwrap(),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                json!(window_locations(
                    case["granted"].as_array().unwrap(),
                    &displays,
                    &case["windows"]
                )),
                case["expected"]
            );
        }
        for case in HELPERS["listApps"].as_array().unwrap() {
            let actual = list_apps_output(
                case["apps"].as_array().unwrap().clone(),
                &case["input"],
                &HashSet::new(),
                "com.example.host",
                case["defaultLocale"].as_str().unwrap(),
            );
            assert_eq!(actual, case["expected"]);
            assert_eq!(
                serde_json::to_string(&actual).unwrap(),
                case["expectedModelText"].as_str().unwrap()
            );
        }
        for case in HELPERS["labels"].as_array().unwrap() {
            assert_eq!(
                safe_label(case["input"].as_str().unwrap())
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                case["expected"]
            );
        }
        for case in HELPERS["ids"].as_array().unwrap() {
            assert_eq!(
                safe_id(case["input"].as_str().unwrap())
                    .map(|s| Value::String(s.into()))
                    .unwrap_or(Value::Null),
                case["expected"]
            );
        }
        for case in HELPERS["tierGuidance"].as_array().unwrap() {
            assert_eq!(
                tier_guidance(case["input"].as_array().unwrap()),
                case["expected"].as_str().unwrap()
            );
        }
        for case in HELPERS["denialGuidance"].as_array().unwrap() {
            let apps = case["apps"].as_array().unwrap();
            if let Some(expected) = case["policyDenied"].as_str() {
                assert_eq!(denied_guidance(apps, true), expected);
            }
            if let Some(expected) = case["userDenied"].as_str() {
                assert_eq!(denied_guidance(apps, false), expected);
            }
            if let Some(expected) = case["notInstalled"].as_str() {
                assert_eq!(
                    not_installed_guidance(apps, case["flags"].as_bool().unwrap()),
                    expected
                );
            }
        }
        for case in HELPERS["suggestions"].as_array().unwrap() {
            let installed = case["installed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|app| AppInfo {
                    bundle_id: app["bundleId"].as_str().unwrap().into(),
                    display_name: app["displayName"].as_str().unwrap().into(),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                json!(suggestions(case["query"].as_str().unwrap(), &installed)),
                case["expected"]
            );
        }
    }
}
