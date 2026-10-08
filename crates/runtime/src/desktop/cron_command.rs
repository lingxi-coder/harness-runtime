//! Desktop-native `/cron` command.
//!
//! An explicit slash invocation is already user intent to manage scheduled
//! prompts. Running it through a model turn would make the cron tools hit the
//! ordinary interactive permission path, which can leave a thin client waiting
//! for a prompt that it cannot render. This handler parses the bounded command
//! grammar and calls the existing cron tools directly, preserving their
//! validation, persistence, scheduler registration, and telemetry.

use command_api::model::BuiltinCommandHandler;
use command_api::{CommandResult, ParsedSlashCommand};
use serde_json::{Value, json};
use tool_api::{BuiltinToolContext, Tool, ToolCallResult, ToolUseContext};

const USAGE: &str = "Usage: /cron <schedule or action>\n\
Create, list, or cancel scheduled prompts.\n\
Examples:\n\
  /cron every weekday at 9am summarize my open pull requests\n\
  /cron 0 9 * * * report the weather in Wuhan\n\
  /cron list\n\
  /cron cancel <job-id>";

#[derive(Debug, PartialEq, Eq)]
enum ParsedCronCommand {
    Usage,
    List,
    Delete(Option<String>),
    Create {
        cron: String,
        prompt: String,
        recurring: bool,
        durable: bool,
    },
}

fn trim_prompt(value: &str) -> &str {
    value.trim_start_matches(&[' ', '\t', ',', '，', ':', '：', '-', '的'][..])
}

fn parse_leading_number(value: &str) -> Option<(u32, &str)> {
    let end = value
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .last()
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    (end > 0)
        .then(|| value[..end].parse::<u32>().ok().map(|n| (n, &value[end..])))
        .flatten()
}

fn parse_chinese_time(value: &str) -> Option<(u32, u32, &str)> {
    let value = value.trim_start();
    let dayparts = [
        ("早上", 9, false),
        ("上午", 9, false),
        ("中午", 12, false),
        ("下午", 15, true),
        ("晚上", 18, true),
        ("夜里", 21, true),
        ("夜间", 21, true),
    ];

    for (label, default_hour, pm) in dayparts {
        let Some(after_label) = value.strip_prefix(label) else {
            continue;
        };
        let after_label = after_label.trim_start();
        let Some((mut hour, after_hour)) = parse_leading_number(after_label) else {
            return Some((0, default_hour, trim_prompt(after_label)));
        };
        let after_hour = after_hour.strip_prefix('点')?;
        let (minute, remainder) = parse_leading_number(after_hour).map_or((0, after_hour), |v| v);
        let remainder = remainder.strip_prefix('分').unwrap_or(remainder);
        if pm && hour < 12 {
            hour += 12;
        }
        return (hour < 24 && minute < 60).then(|| (minute, hour, trim_prompt(remainder)));
    }

    let (hour, after_hour) = parse_leading_number(value)?;
    let after_hour = after_hour.strip_prefix('点')?;
    let (minute, remainder) = parse_leading_number(after_hour).map_or((0, after_hour), |v| v);
    let remainder = remainder.strip_prefix('分').unwrap_or(remainder);
    (hour < 24 && minute < 60).then(|| (minute, hour, trim_prompt(remainder)))
}

fn parse_english_clock_token(token: &str) -> Option<(u32, u32)> {
    let lower = token.to_ascii_lowercase();
    let (clock, pm, has_meridiem) = if let Some(clock) = lower.strip_suffix("am") {
        (clock, false, true)
    } else if let Some(clock) = lower.strip_suffix("pm") {
        (clock, true, true)
    } else {
        (lower.as_str(), false, false)
    };
    let (hour_raw, minute_raw) = clock.split_once(':').unwrap_or((clock, "0"));
    let mut hour = hour_raw.parse::<u32>().ok()?;
    let minute = minute_raw.parse::<u32>().ok()?;
    if has_meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        if pm && hour != 12 {
            hour += 12;
        } else if !pm && hour == 12 {
            hour = 0;
        }
    }
    (hour < 24 && minute < 60).then_some((minute, hour))
}

fn parse_english_time(value: &str) -> Option<(u32, u32, &str)> {
    let value = value.trim_start();
    let value = value.strip_prefix("at ").unwrap_or(value);
    let lower = value.to_ascii_lowercase();
    for (label, hour) in [
        ("morning", 9),
        ("noon", 12),
        ("afternoon", 15),
        ("evening", 18),
        ("night", 21),
    ] {
        if lower.starts_with(label) {
            return Some((0, hour, trim_prompt(&value[label.len()..])));
        }
    }

    let token_end = value.find(char::is_whitespace).unwrap_or(value.len());
    let token = &value[..token_end];
    let (minute, hour) = parse_english_clock_token(token)?;
    Some((minute, hour, trim_prompt(&value[token_end..])))
}

fn parse_raw_cron(args: &str) -> Option<Result<ParsedCronCommand, String>> {
    let fields = args.split_whitespace().take(5).collect::<Vec<_>>();
    if fields.len() != 5 {
        return None;
    }
    let expression = fields.join(" ");
    if cron::parse_cron(&expression).is_err() {
        return None;
    }
    let prompt = args
        .split_whitespace()
        .skip(5)
        .collect::<Vec<_>>()
        .join(" ");
    if prompt.is_empty() {
        return Some(Err(
            "A prompt is required after the five-field cron expression.".into(),
        ));
    }
    let lower = prompt.to_ascii_lowercase();
    let recurring = !lower.contains("one-shot")
        && !lower.contains("run once")
        && !prompt.contains("仅一次")
        && !prompt.contains("单次");
    let durable = !lower.contains("session-only")
        && !lower.contains("temporary")
        && !prompt.contains("临时")
        && !prompt.contains("仅本次会话");
    Some(Ok(ParsedCronCommand::Create {
        cron: expression,
        prompt,
        recurring,
        durable,
    }))
}

fn parse_natural_schedule(args: &str) -> Option<Result<ParsedCronCommand, String>> {
    for (prefix, day_of_week) in [
        ("每个工作日", "1-5"),
        ("每周一到周五", "1-5"),
        ("工作日", "1-5"),
        ("每天", "*"),
        ("每日", "*"),
    ] {
        let Some(rest) = args.strip_prefix(prefix) else {
            continue;
        };
        let Some((minute, hour, prompt)) = parse_chinese_time(rest) else {
            return Some(Err(format!(
                "Could not determine the time after '{prefix}'. Try '{prefix}早上 ...' or provide a five-field cron expression."
            )));
        };
        if prompt.is_empty() {
            return Some(Err("定时执行的任务内容不能为空。".into()));
        }
        return Some(Ok(ParsedCronCommand::Create {
            cron: format!("{minute} {hour} * * {day_of_week}"),
            prompt: prompt.to_string(),
            recurring: true,
            durable: true,
        }));
    }

    let lower = args.to_ascii_lowercase();
    for (prefix, day_of_week) in [
        ("every weekday", "1-5"),
        ("weekdays", "1-5"),
        ("every day", "*"),
        ("daily", "*"),
    ] {
        let Some(rest_lower) = lower.strip_prefix(prefix) else {
            continue;
        };
        let consumed = args.len() - rest_lower.len();
        let rest = &args[consumed..];
        let Some((minute, hour, prompt)) = parse_english_time(rest) else {
            return Some(Err(format!(
                "Could not determine the time after '{prefix}'. Try '{prefix} at 9am ...' or provide a five-field cron expression."
            )));
        };
        if prompt.is_empty() {
            return Some(Err("The scheduled prompt cannot be empty.".into()));
        }
        return Some(Ok(ParsedCronCommand::Create {
            cron: format!("{minute} {hour} * * {day_of_week}"),
            prompt: prompt.to_string(),
            recurring: true,
            durable: true,
        }));
    }
    None
}

fn parse_cron_command(args: &str) -> Result<ParsedCronCommand, String> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(ParsedCronCommand::Usage);
    }

    let lower = args.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "list" | "status" | "show" | "jobs" | "列表" | "列出" | "查看" | "状态"
    ) {
        return Ok(ParsedCronCommand::List);
    }

    for prefix in ["cancel", "delete", "stop", "取消", "删除", "停止"] {
        if lower == prefix {
            return Ok(ParsedCronCommand::Delete(None));
        }
        if lower.starts_with(prefix)
            && lower[prefix.len()..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            let id = args[prefix.len()..].trim();
            return Ok(ParsedCronCommand::Delete(
                (!id.is_empty()).then(|| id.to_string()),
            ));
        }
    }

    if let Some(parsed) = parse_raw_cron(args) {
        return parsed;
    }
    if let Some(parsed) = parse_natural_schedule(args) {
        return parsed;
    }

    Err("Could not understand the schedule. Use a form such as 'every day at 9am ...', '每天早上...', or begin with a five-field cron expression.".into())
}

fn tool_result_text(result: ToolCallResult) -> String {
    result
        .model_content
        .or_else(|| {
            result
                .data
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            result
                .data
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| result.data.to_string())
}

pub(crate) struct DesktopCronCommandHandler {
    ctx: BuiltinToolContext,
}

impl DesktopCronCommandHandler {
    pub(crate) fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn call_tool<T: Tool + Sync>(&self, tool: &T, input: Value) -> Result<String, String> {
        let call_ctx = ToolUseContext::model_seed(self.ctx.default_model.clone(), None);
        tool.validate_input(&input, &call_ctx)
            .await
            .map_err(|error| error.to_string())?;
        let (progress, _rx) = tool_api::progress_channel();
        tool.call(input, call_ctx, progress)
            .await
            .map(tool_result_text)
            .map_err(|error| error.to_string())
    }

    async fn list(&self) -> Result<String, String> {
        self.call_tool(&tool_cron::CronListTool::new(self.ctx.clone()), json!({}))
            .await
    }
}

#[async_trait::async_trait]
impl BuiltinCommandHandler for DesktopCronCommandHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let display = match parse_cron_command(&args.raw_args) {
            Ok(ParsedCronCommand::Usage) => USAGE.to_string(),
            Ok(ParsedCronCommand::List) => self.list().await.unwrap_or_else(|error| error),
            Ok(ParsedCronCommand::Delete(None)) => match self.list().await {
                Ok(jobs) => format!("{jobs}\n\nChoose a job ID and run `/cron cancel <job-id>`."),
                Err(error) => error,
            },
            Ok(ParsedCronCommand::Delete(Some(id))) => self
                .call_tool(
                    &tool_cron::CronDeleteTool::new(self.ctx.clone()),
                    json!({ "id": id }),
                )
                .await
                .unwrap_or_else(|error| error),
            Ok(ParsedCronCommand::Create {
                cron,
                prompt,
                recurring,
                durable,
            }) => self
                .call_tool(
                    &tool_cron::CronCreateTool::new(self.ctx.clone()),
                    json!({
                        "cron": cron,
                        "prompt": prompt,
                        "recurring": recurring,
                        "durable": durable,
                    }),
                )
                .await
                .unwrap_or_else(|error| error),
            Err(error) => error,
        };
        CommandResult::Done {
            display: Some(display),
        }
    }

    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Create, list, or cancel scheduled prompts"
    }

    fn argument_hint(&self) -> Option<&str> {
        Some("<schedule or action>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chinese_daily_morning_request() {
        assert_eq!(
            parse_cron_command("每天早上汇报武汉天气"),
            Ok(ParsedCronCommand::Create {
                cron: "0 9 * * *".into(),
                prompt: "汇报武汉天气".into(),
                recurring: true,
                durable: true,
            })
        );
    }

    #[test]
    fn parses_raw_cron_and_management_actions() {
        assert_eq!(
            parse_cron_command("30 14 * * 1-5 summarize pull requests"),
            Ok(ParsedCronCommand::Create {
                cron: "30 14 * * 1-5".into(),
                prompt: "summarize pull requests".into(),
                recurring: true,
                durable: true,
            })
        );
        assert_eq!(parse_cron_command("list"), Ok(ParsedCronCommand::List));
        assert_eq!(
            parse_cron_command("cancel job-123"),
            Ok(ParsedCronCommand::Delete(Some("job-123".into())))
        );
    }

    #[test]
    fn parses_english_weekday_schedule() {
        assert_eq!(
            parse_cron_command("every weekday at 9:30am summarize my open pull requests"),
            Ok(ParsedCronCommand::Create {
                cron: "30 9 * * 1-5".into(),
                prompt: "summarize my open pull requests".into(),
                recurring: true,
                durable: true,
            })
        );
    }

    #[test]
    fn rejects_missing_prompt() {
        assert!(parse_cron_command("0 9 * * *")
            .expect_err("missing prompt must fail")
            .contains("prompt"));
    }

    #[test]
    fn exposes_desktop_command_metadata() {
        let handler = DesktopCronCommandHandler::new(tool_api::test_support::shell_test_ctx(
            mobile_linux_api::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
        ));
        assert_eq!(handler.name(), "cron");
        assert_eq!(handler.argument_hint(), Some("<schedule or action>"));
    }

    #[tokio::test]
    async fn explicit_command_persists_without_a_permission_round_trip() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let ctx = tool_api::test_support::shell_test_ctx_in(
            mobile_linux_api::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
            workspace.path().to_path_buf(),
        );
        let handler = DesktopCronCommandHandler::new(ctx);
        let result = handler
            .handle(&ParsedSlashCommand {
                name: "cron".into(),
                raw_args: "每天早上汇报武汉天气".into(),
                positional_args: vec![],
            })
            .await;

        let CommandResult::Done {
            display: Some(display),
        } = result
        else {
            panic!("cron must complete as a local command");
        };
        assert!(display.contains("Scheduled recurring job"));

        let path = cron::tasks_file::session_scheduled_tasks_path(workspace.path());
        let body = std::fs::read_to_string(path).expect("durable session cron file");
        assert!(
            !cron::tasks_file::scheduled_tasks_path(workspace.path()).exists(),
            "/cron must not write the independent task-center store"
        );
        let tasks = cron::tasks_file::parse_tasks(&body).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].cron, "0 9 * * *");
        assert_eq!(tasks[0].prompt, "汇报武汉天气");
    }
}
