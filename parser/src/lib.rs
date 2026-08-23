//! Incremental HTML tokenization and tree construction for Wild Buzzard.
//!
//! This is a deliberately bounded wave-one subset of the WHATWG parser, not a
//! claim of full html5lib parity. It implements the insertion modes and token
//! states needed by ordinary static documents while preserving structured
//! errors for unsupported or malformed input.

mod source;
mod tokenizer;
mod tree_builder;

pub use source::{SourcePosition, SourceSpan};
pub use tokenizer::{
    ParseError, ParseErrorCode, ParsePhase, SpannedAttribute, SpannedToken, Token, Tokenizer,
    TokenizerLimits, TokenizerStateError,
};
pub use tree_builder::{
    DocumentMode, HtmlParser, ParseOutput, ParserInsertedScript, ParserScriptStartTag,
    ParserStateError, ScriptHandlerError, parse_document,
};
