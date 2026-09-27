//! How caller `args` reach the workflow VM.
//!
//! `run_with_progress` used to splice the caller's JSON into
//! `globalThis.args = <json>;` as an object literal. Two properties of that
//! choice were reviewed; one was a real defect and one was not, and both are
//! pinned here so neither is re-litigated from the source alone.

use serde_json::json;

fn run(script: &str, args: Option<String>) -> Result<Option<String>, String> {
    let outcome = workflow::run_with_progress(
        script,
        |prompts, _options| prompts.iter().map(|_| "{}".to_string()).collect(),
        |_progress| {},
        None,
        true,
        args,
        None,
    );
    match outcome {
        Ok(o) => Ok(o.result),
        Err(e) => Err(format!("{e:?}")),
    }
}

const META: &str = "export const meta = { name: 'args-probe', description: 'args probe' }\n";

/// REGRESSION: `args` must be `JSON.parse`d, never spliced in as an object
/// literal.
///
/// In an object literal `__proto__:` is a prototype setter, not an own
/// property. Under the old `globalThis.args = {json};` this exact input
/// produced `Object.keys(args) == ["ok"]` and
/// `hasOwnProperty(args, 'polluted') == false` while `args.polluted` still read
/// back `true` — so a script that allow-lists its input by enumerating keys
/// admits a value it never saw. `JSON.parse` makes the key ordinary data, so
/// enumeration and property access agree.
///
/// The scope claim is asserted too, in both directions: this was never global
/// `Object.prototype` pollution. It reached the `args` object only, which is
/// why it is a gate bypass rather than a sandbox escape.
#[test]
fn proto_key_in_args_is_an_ordinary_own_property() {
    let args = json!({"__proto__": {"polluted": true}, "ok": 1}).to_string();
    let script = format!(
        "{META}return JSON.stringify({{ \
           keys: Object.keys(args), \
           has_own_polluted: Object.prototype.hasOwnProperty.call(args, 'polluted'), \
           reads_polluted: args.polluted === true, \
           has_own_proto: Object.prototype.hasOwnProperty.call(args, '__proto__'), \
           object_prototype_clean: ({{}}).polluted === undefined, \
           ok: args.ok, \
         }})"
    );
    let result = run(&script, Some(args)).expect("the run must succeed");
    let result = result.expect("the script must return a value");
    let parsed: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&result).unwrap_or(result.clone()))
            .unwrap_or_else(|_| serde_json::from_str(&result).expect("result must be JSON"));

    assert_eq!(
        parsed["reads_polluted"], false,
        "args.polluted must NOT resolve: a `__proto__` key that is invisible to \
         Object.keys while still readable through the prototype chain is the \
         whole defect, got {parsed}"
    );
    assert_eq!(
        parsed["has_own_proto"], true,
        "`__proto__` must arrive as an ordinary OWN property, so a script's own \
         key allow-list can see and reject it, got {parsed}"
    );
    assert_eq!(
        parsed["keys"],
        json!(["__proto__", "ok"]),
        "both keys must be enumerable, got {parsed}"
    );
    assert_eq!(
        parsed["ok"], 1,
        "vacuity guard: ordinary keys must still arrive intact, got {parsed}"
    );
    assert_eq!(
        parsed["object_prototype_clean"], true,
        "this was never global prototype pollution; if that ever changes it is a \
         different and much worse bug, got {parsed}"
    );
}

/// NOT a defect, pinned so the refutation is not lost: U+2028 and U+2029 are
/// line terminators in pre-ES2019 string-literal parsing, and `serde_json`
/// emits them raw. This engine implements the ES2019 JSON-superset, so they
/// stay inside the literal.
///
/// The second case is the escalation that would matter if it did not: a payload
/// that closes the string and appends code. It must not execute.
#[test]
fn line_separators_in_args_stay_inside_the_string() {
    let args = json!({"name": "my\u{2028}app", "tail": "sentinel"}).to_string();
    assert!(
        args.contains('\u{2028}'),
        "fixture guard: serde_json must be emitting U+2028 RAW, or this test \
         proves nothing about the engine"
    );
    let script =
        format!("{META}return JSON.stringify({{ name_len: args.name.length, tail: args.tail }})");
    let result = run(&script, Some(args))
        .expect("a U+2028 in a string value must not break the run")
        .expect("the script must return a value");
    assert!(
        result.contains("\\\"name_len\\\":6") || result.contains("\"name_len\":6"),
        "the string must survive whole — m,y,U+2028,a,p,p — got {result}"
    );
    assert!(
        result.contains("sentinel"),
        "the key after the U+2028 must still parse, got {result}"
    );

    let payload = format!("x{}\u{2028};globalThis.__pwned=1;var _z=\"", '"');
    let args = json!({ "name": payload }).to_string();
    let script = format!("{META}return JSON.stringify({{ pwned: globalThis.__pwned === 1 }})");
    let result = run(&script, Some(args))
        .expect("the breakout payload must not fail the run")
        .expect("the script must return a value");
    assert!(
        result.contains("false"),
        "a U+2028 must not terminate the string literal and let the rest run as \
         code, got {result}"
    );
}
