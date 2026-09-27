//! Host projections over the independent client codecs.
//! No provider protocol implementation lives in this module.
pub mod gemini_files;
pub use crate::upstream::{
    AnthropicMessagesCodec, AzureOpenAiCodec, BedrockClaudeCodec, FoundryClaudeCodec, GeminiCodec,
    OpenAiChatCodec, OpenAiResponsesCodec, VertexClaudeCodec, VertexGeminiCodec,
};
pub use gemini_files::GeminiFile;
