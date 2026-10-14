//! The native Claude Code 2.1.299 argument counter shared by client composers.
//!
//! Keep the parser in JavaScript: shell-word approximations disagree on command
//! substitutions, declarations, incomplete quotes, and the first command. The
//! checked-in source and differential corpus are pinned in `docs/parity/`.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct NativeArgumentParser {
    context: rquickjs::Context,
    // The context must be dropped before its runtime.
    _runtime: rquickjs::Runtime,
    deadline: Arc<Mutex<Instant>>,
    last_count: Option<(String, u32)>,
}

impl NativeArgumentParser {
    fn new() -> Option<Self> {
        let runtime = rquickjs::Runtime::new().ok()?;
        runtime.set_memory_limit(32 * 1024 * 1024);
        runtime.set_max_stack_size(512 * 1024);
        let deadline = Arc::new(Mutex::new(Instant::now() + Duration::from_secs(1)));
        let guard = Arc::clone(&deadline);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            guard.lock().map_or(true, |limit| Instant::now() >= *limit)
        })));
        let context = rquickjs::Context::full(&runtime).ok()?;
        context
            .with(|ctx| ctx.eval::<(), _>(include_str!("slash_argument_parser_299.js")))
            .ok()?;
        Some(Self {
            context,
            _runtime: runtime,
            deadline,
            last_count: None,
        })
    }

    fn count(&mut self, args: &str) -> Option<u32> {
        if let Some((previous, count)) = &self.last_count {
            if previous == args {
                return Some(*count);
            }
        }
        *self.deadline.lock().ok()? = Instant::now() + Duration::from_secs(1);
        let count = self.context.with(|ctx| {
            let function: rquickjs::Function =
                ctx.globals().get("slashCommandArgumentCount").ok()?;
            function.call((args,)).ok()
        })?;
        self.last_count = Some((args.into(), count));
        Some(count)
    }
}

thread_local! {
    static PARSER: RefCell<Option<NativeArgumentParser>> = const { RefCell::new(None) };
}

/// Count completed arguments using the exact native slash-hint parser.
///
/// This only parses text; it never executes the input or exposes host bindings.
/// `None` suppresses the decorative hint if the bounded parser cannot run.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[must_use]
pub fn slash_command_argument_count(args: String) -> Option<u32> {
    PARSER.with(|slot| {
        let mut parser = slot.try_borrow_mut().ok()?;
        if parser.is_none() {
            *parser = NativeArgumentParser::new();
        }
        let count = parser.as_mut()?.count(&args);
        if count.is_none() {
            // Discard an interrupted/failed VM before the next keystroke.
            *parser = None;
        }
        count
    })
}

#[cfg(test)]
mod tests {
    use super::slash_command_argument_count;

    #[test]
    fn native_argument_counter_matches_extracted_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/parity/slash-argument-hints-2.1.299.json"
        ))
        .unwrap();
        let cases = fixture["argumentCases"]
            .as_array()
            .expect("native parser cases");
        assert!(cases.len() >= 81);
        for case in cases {
            let input = case["input"].as_str().unwrap();
            let expected = case["count"].as_u64().unwrap() as u32;
            assert_eq!(
                slash_command_argument_count(input.into()),
                Some(expected),
                "input length {}: {:?}",
                input.len(),
                input.chars().take(80).collect::<String>()
            );
        }
    }

    #[test]
    fn native_argument_counter_reuses_context_without_state_leaks() {
        for _ in 0..3 {
            assert_eq!(
                slash_command_argument_count("one 'two three' ".into()),
                Some(2)
            );
            assert_eq!(slash_command_argument_count(String::new()), Some(0));
            assert_eq!(
                slash_command_argument_count("one; two three".into()),
                Some(1)
            );
        }
    }
}
