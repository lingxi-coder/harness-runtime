//! Transport-neutral DTOs for an interactive `AskUserQuestion` round-trip.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

use super::text::{decode_json_string, encode_json_string, ClientTextDto};

fn array_index_key(code_units: &[u16]) -> Option<u32> {
    if code_units.is_empty() || (code_units.len() > 1 && code_units[0] == u16::from(b'0')) {
        return None;
    }
    let mut value = 0u64;
    for unit in code_units {
        if !(u16::from(b'0')..=u16::from(b'9')).contains(unit) {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(*unit - u16::from(b'0')))?;
        if value > 4_294_967_294 {
            return None;
        }
    }
    Some(value as u32)
}

/// One answer pair in the UniFFI-facing ordered representation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskUserQuestionAnswerDto {
    /// Exact question text used as the answer object's JSON key.
    pub question: ClientTextDto,
    /// Exact selected label or custom answer.
    pub answer: ClientTextDto,
}

/// Ordered answer pairs internally, serialized as the existing ordinary JSON
/// object on the client protocol wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskUserQuestionAnswersDto {
    /// Pairs in the order supplied by the user interface.
    pub pairs: Vec<AskUserQuestionAnswerDto>,
}

impl Serialize for AskUserQuestionAnswersDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut unique = Vec::<(&ClientTextDto, &ClientTextDto)>::new();
        for pair in &self.pairs {
            if let Some(existing) = unique
                .iter()
                .position(|(question, _)| question.code_units == pair.question.code_units)
            {
                // Preserve the previous map behavior: repeated question keys
                // keep their first position and the last selected answer.
                unique[existing].1 = &pair.answer;
            } else {
                unique.push((&pair.question, &pair.answer));
            }
        }
        let mut indexed = Vec::new();
        let mut ordinary = Vec::new();
        for pair in unique {
            if let Some(index) = array_index_key(&pair.0.code_units) {
                indexed.push((index, pair));
            } else {
                ordinary.push(pair);
            }
        }
        indexed.sort_by_key(|(index, _)| *index);

        let mut json = String::from("{");
        for (index, (question, answer)) in indexed
            .into_iter()
            .map(|(_, pair)| pair)
            .chain(ordinary)
            .enumerate()
        {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&encode_json_string(&question.code_units));
            json.push(':');
            json.push_str(&encode_json_string(&answer.code_units));
        }
        json.push('}');
        let raw = RawValue::from_string(json).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AskUserQuestionAnswersDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        decode_answer_object(raw.get()).map_err(serde::de::Error::custom)
    }
}

/// One selectable answer shown by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskOptionDto {
    /// Short display label.
    pub label: ClientTextDto,
    /// Explanation of the option's effect.
    pub description: ClientTextDto,
    /// Optional focused-option preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<ClientTextDto>,
}

/// One question in an interactive questionnaire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskQuestionDto {
    /// Complete question text and answer-object key.
    pub question: ClientTextDto,
    /// Short section label.
    pub header: ClientTextDto,
    /// Declared answer options. Clients synthesize the free-text `Other` row.
    pub options: Vec<AskOptionDto>,
    /// Whether more than one option may be selected.
    pub multi_select: bool,
}

/// One outbound interactive `AskUserQuestion` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskUserQuestionRequestDto {
    /// Connection-scoped correlator echoed by the answer/cancel command.
    pub request_id: u64,
    /// Questions in display order.
    pub questions: Vec<AskQuestionDto>,
    /// Idle auto-continue window in seconds; `None` means wait indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

fn decode_answer_object(raw: &str) -> Result<AskUserQuestionAnswersDto, String> {
    let bytes = raw.as_bytes();
    let mut cursor = JsonCursor { raw, bytes, at: 0 };
    cursor.whitespace();
    cursor.expect(b'{')?;
    cursor.whitespace();
    let mut pairs: Vec<AskUserQuestionAnswerDto> = Vec::new();
    if cursor.consume(b'}') {
        cursor.finish()?;
        return Ok(AskUserQuestionAnswersDto { pairs });
    }
    loop {
        cursor.whitespace();
        let question = cursor.string()?;
        cursor.whitespace();
        cursor.expect(b':')?;
        cursor.whitespace();
        let answer = cursor.string()?;
        if let Some(existing) = pairs
            .iter()
            .position(|pair| pair.question.code_units == question.code_units)
        {
            // JSON object parsing keeps the first property position and the
            // last value when a source object repeats a property name.
            pairs[existing].answer = answer;
        } else {
            pairs.push(AskUserQuestionAnswerDto { question, answer });
        }
        cursor.whitespace();
        if cursor.consume(b'}') {
            break;
        }
        cursor.expect(b',')?;
    }
    cursor.finish()?;
    Ok(AskUserQuestionAnswersDto { pairs })
}

struct JsonCursor<'a> {
    raw: &'a str,
    bytes: &'a [u8],
    at: usize,
}

impl JsonCursor<'_> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.at += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.at) == Some(&expected) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        self.consume(expected).then_some(()).ok_or_else(|| {
            format!(
                "expected `{}` in AskUserQuestion answers object",
                expected as char
            )
        })
    }

    fn string(&mut self) -> Result<ClientTextDto, String> {
        let start = self.at;
        self.expect(b'"')?;
        while let Some(byte) = self.bytes.get(self.at).copied() {
            match byte {
                b'"' => {
                    self.at += 1;
                    return decode_json_string(&self.raw[start..self.at]);
                }
                b'\\' => {
                    self.at += 1;
                    match self.bytes.get(self.at).copied() {
                        Some(b'u') => self.at = self.at.saturating_add(5),
                        Some(_) => self.at += 1,
                        None => return Err("unterminated AskUserQuestion string escape".into()),
                    }
                }
                0..=0x1f => return Err("unescaped control in AskUserQuestion string".into()),
                _ if byte < 0x80 => self.at += 1,
                _ => {
                    let character = self.raw[self.at..]
                        .chars()
                        .next()
                        .ok_or_else(|| "invalid UTF-8 in AskUserQuestion string".to_string())?;
                    self.at += character.len_utf8();
                }
            }
        }
        Err("unterminated AskUserQuestion JSON string".into())
    }

    fn finish(&mut self) -> Result<(), String> {
        self.whitespace();
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing data after AskUserQuestion answers object".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_text_round_trips_as_an_ordinary_json_string() {
        let text = ClientTextDto {
            code_units: vec![b'Q' as u16, 0xd800, b'!' as u16],
        };
        let encoded = serde_json::to_string(&text).unwrap();
        assert_eq!(encoded, r#""Q\ud800!""#);
        let decoded: ClientTextDto = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, text);
    }

    #[test]
    fn answers_keep_normal_object_shape_and_exact_keys_and_values() {
        let answers = AskUserQuestionAnswersDto {
            pairs: vec![AskUserQuestionAnswerDto {
                question: ClientTextDto {
                    code_units: vec![b'Q' as u16, 0xd800],
                },
                answer: ClientTextDto {
                    code_units: vec![b'A' as u16, 0xdc00],
                },
            }],
        };
        let encoded = serde_json::to_string(&answers).unwrap();
        assert_eq!(encoded, r#"{"Q\ud800":"A\udc00"}"#);
        let decoded: AskUserQuestionAnswersDto = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, answers);
    }

    #[test]
    fn answers_preserve_pair_order_in_the_ffi_representation() {
        let answers: AskUserQuestionAnswersDto =
            serde_json::from_str(r#"{"second":"B","first":"A"}"#).unwrap();
        let keys = answers
            .pairs
            .iter()
            .map(|pair| pair.question.display())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["second", "first"]);
    }

    #[test]
    fn answer_wire_uses_native_integer_key_order_without_reordering_ffi_pairs() {
        let answers: AskUserQuestionAnswersDto = serde_json::from_str(
            r#"{"10":"ten","2":"two","01":"zero-one","x":"ex","Q\ud800":"lone","1":"one","4294967294":"last-index","4294967295":"not-index"}"#,
        )
        .unwrap();
        assert_eq!(
            answers
                .pairs
                .iter()
                .map(|pair| pair.question.display())
                .collect::<Vec<_>>(),
            ["10", "2", "01", "x", "Q�", "1", "4294967294", "4294967295"]
        );
        assert_eq!(answers.pairs[4].question.code_units, [b'Q' as u16, 0xd800]);
        assert_eq!(
            serde_json::to_string(&answers).unwrap(),
            r#"{"1":"one","2":"two","10":"ten","4294967294":"last-index","01":"zero-one","x":"ex","Q\ud800":"lone","4294967295":"not-index"}"#
        );
    }

    #[test]
    fn answer_command_keeps_the_existing_object_wire_shape_with_exact_units() {
        let raw =
            r#"{"type":"answer_ask_user_question","request_id":8,"answers":{"Q\ud800":"A\udc00"}}"#;
        let command: crate::protocol::commands::ClientCommand = serde_json::from_str(raw).unwrap();
        let crate::protocol::commands::ClientCommand::AnswerAskUserQuestion { answers, .. } =
            command
        else {
            panic!("expected AskUserQuestion answer command");
        };
        assert_eq!(answers.pairs.len(), 1);
        assert_eq!(answers.pairs[0].question.code_units, [b'Q' as u16, 0xd800]);
        assert_eq!(answers.pairs[0].answer.code_units, [b'A' as u16, 0xdc00]);

        let command = crate::protocol::commands::ClientCommand::AnswerAskUserQuestion {
            request_id: 8,
            answers,
        };
        let encoded = serde_json::to_string(&command).unwrap();
        assert!(encoded.contains(r#""answers":{"Q\ud800":"A\udc00"}"#));
        assert!(!encoded.contains("\"pairs\""));

        // Exercise the actual tagged protocol envelope, not only the DTO.
        let decoded: crate::protocol::commands::ClientCommand =
            serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, command);

        // Non-Ask variants still use their established serde behavior.
        let cancel: crate::protocol::commands::ClientCommand =
            serde_json::from_str(r#"{"type":"cancel","turn_id":7}"#).unwrap();
        assert_eq!(
            cancel,
            crate::protocol::commands::ClientCommand::Cancel { turn_id: Some(7) }
        );
    }

    #[test]
    fn ask_user_question_event_envelope_round_trips_exact_utf16_fields() {
        let raw = r#"{"type":"ask_user_question","request":{"request_id":4,"questions":[{"question":"Q\ud800","header":"Pick","options":[{"label":"A\udfff","description":"First"}],"multi_select":false}]}}"#;
        let event: crate::protocol::events::ClientEvent = serde_json::from_str(raw).unwrap();
        let crate::protocol::events::ClientEvent::AskUserQuestion { request } = &event else {
            panic!("expected AskUserQuestion event");
        };
        assert_eq!(
            request.questions[0].question.code_units,
            [b'Q' as u16, 0xd800]
        );
        assert_eq!(
            request.questions[0].options[0].label.code_units,
            [b'A' as u16, 0xdfff]
        );

        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains(r#""question":"Q\ud800""#));
        assert!(encoded.contains(r#""label":"A\udfff""#));
        assert!(!encoded.contains("code_units"));
        assert!(!encoded.contains("root"));
        let decoded: crate::protocol::events::ClientEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);

        // A different event tag remains on the legacy derived path.
        let resolved: crate::protocol::events::ClientEvent =
            serde_json::from_str(r#"{"type":"ask_user_question_resolved","request_id":5}"#)
                .unwrap();
        assert_eq!(
            resolved,
            crate::protocol::events::ClientEvent::AskUserQuestionResolved { request_id: 5 }
        );
    }

    #[test]
    fn request_dtos_keep_plain_string_fields_and_exact_option_units() {
        let request = AskUserQuestionRequestDto {
            request_id: 4,
            questions: vec![AskQuestionDto {
                question: ClientTextDto {
                    code_units: vec![b'Q' as u16, 0xd800],
                },
                header: ClientTextDto::from_utf8("Pick"),
                options: vec![AskOptionDto {
                    label: ClientTextDto {
                        code_units: vec![b'A' as u16, 0xdc00],
                    },
                    description: ClientTextDto::from_utf8("First"),
                    preview: None,
                }],
                multi_select: false,
            }],
            timeout_secs: None,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains(r#""question":"Q\ud800""#));
        assert!(encoded.contains(r#""label":"A\udc00""#));
        let decoded: AskUserQuestionRequestDto = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
    }
}
