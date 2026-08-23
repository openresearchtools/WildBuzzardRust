use std::{convert::Infallible, fmt};

use wild_buzzard_dom::{AttributeName, Document, DocumentVersion, DomError, NodeId, NodeKind};

use crate::source::{SourcePosition, SourceSpan};
use crate::tokenizer::{
    ParseError, ParseErrorCode, ParsePhase, SpannedAttribute, SpannedToken, Token, Tokenizer,
    TokenizerLimits, TokenizerStateError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentMode {
    NoQuirks,
    Quirks,
}

#[derive(Debug)]
pub struct ParseOutput {
    pub document: Document,
    pub errors: Vec<ParseError>,
    pub document_mode: DocumentMode,
    completion: ParserCompletionProof,
}

/// Nonforgeable evidence binding one finished parser to its exact document
/// revision and every synchronously dispatched closed-script boundary.
///
/// The browser's parser/DOM lease consumes this evidence at final publication.
/// A caller may inspect the counts, but cannot manufacture a completion for a
/// document whose script callbacks were ignored by the browser host.
#[derive(Debug)]
pub struct ParserCompletionProof {
    document_version: DocumentVersion,
    script_boundaries: u64,
}

impl ParseOutput {
    /// Exact document revision validated when parsing finished.
    #[must_use]
    pub const fn completion_document_version(&self) -> DocumentVersion {
        self.completion.document_version
    }

    /// Number of closed parser-inserted script boundaries whose callback
    /// returned normally before this parse completed.
    #[must_use]
    pub const fn completed_script_boundaries(&self) -> u64 {
        self.completion.script_boundaries
    }
}

/// Execution-affecting parser state captured when a script start tag is inserted.
///
/// Inline source text is deliberately absent: the browser reads that text from
/// the live element only after the pre-script microtask checkpoint. Attributes
/// and the first applicable base `href` are start-tag state and cannot be
/// replaced by DOM mutations performed during that checkpoint.
#[derive(Clone, Eq, PartialEq)]
pub struct ParserScriptStartTag {
    opening_span: SourceSpan,
    base_href: Option<String>,
    src: Option<String>,
    script_type: Option<String>,
    language: Option<String>,
    charset: Option<String>,
    cross_origin: Option<String>,
    integrity: Option<String>,
    nonce: Option<String>,
    referrer_policy: Option<String>,
    fetch_priority: Option<String>,
    blocking: Option<String>,
    async_present: bool,
    defer_present: bool,
    no_module_present: bool,
}

impl fmt::Debug for ParserScriptStartTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParserScriptStartTag")
            .field("opening_span", &self.opening_span)
            .field("base_href_present", &self.base_href.is_some())
            .field("src_present", &self.src.is_some())
            .field("type_present", &self.script_type.is_some())
            .field("language_present", &self.language.is_some())
            .field("charset_present", &self.charset.is_some())
            .field("cross_origin_present", &self.cross_origin.is_some())
            .field("integrity_present", &self.integrity.is_some())
            .field("nonce_present", &self.nonce.is_some())
            .field("referrer_policy_present", &self.referrer_policy.is_some())
            .field("fetch_priority_present", &self.fetch_priority.is_some())
            .field("blocking_present", &self.blocking.is_some())
            .field("async_present", &self.async_present)
            .field("defer_present", &self.defer_present)
            .field("no_module_present", &self.no_module_present)
            .finish()
    }
}

impl ParserScriptStartTag {
    #[must_use]
    pub const fn opening_span(&self) -> SourceSpan {
        self.opening_span
    }

    #[must_use]
    pub fn base_href(&self) -> Option<&str> {
        self.base_href.as_deref()
    }

    #[must_use]
    pub fn src(&self) -> Option<&str> {
        self.src.as_deref()
    }

    #[must_use]
    pub fn script_type(&self) -> Option<&str> {
        self.script_type.as_deref()
    }

    #[must_use]
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    #[must_use]
    pub fn charset(&self) -> Option<&str> {
        self.charset.as_deref()
    }

    #[must_use]
    pub fn cross_origin(&self) -> Option<&str> {
        self.cross_origin.as_deref()
    }

    #[must_use]
    pub fn integrity(&self) -> Option<&str> {
        self.integrity.as_deref()
    }

    #[must_use]
    pub fn nonce(&self) -> Option<&str> {
        self.nonce.as_deref()
    }

    #[must_use]
    pub fn referrer_policy(&self) -> Option<&str> {
        self.referrer_policy.as_deref()
    }

    #[must_use]
    pub fn fetch_priority(&self) -> Option<&str> {
        self.fetch_priority.as_deref()
    }

    #[must_use]
    pub fn blocking(&self) -> Option<&str> {
        self.blocking.as_deref()
    }

    #[must_use]
    pub const fn async_present(&self) -> bool {
        self.async_present
    }

    #[must_use]
    pub const fn defer_present(&self) -> bool {
        self.defer_present
    }

    #[must_use]
    pub const fn no_module_present(&self) -> bool {
        self.no_module_present
    }
}

#[derive(Debug)]
struct PendingParserScript {
    node: NodeId,
    start_tag: ParserScriptStartTag,
}

/// Parser-inserted script boundary observed immediately after its end tag.
///
/// The following input token has not been processed when this value is issued.
/// Inline source remains live in the supplied document for preparation after
/// the pre-script checkpoint. Execution attributes are the immutable start-tag
/// snapshot exposed by [`Self::start_tag`].
#[derive(Debug, Eq, PartialEq)]
pub struct ParserInsertedScript {
    node: NodeId,
    document_version: DocumentVersion,
    ordinal: u64,
    closing_span: SourceSpan,
    start_tag: ParserScriptStartTag,
}

impl ParserInsertedScript {
    /// Exact script element created by this parser.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// Exact live document revision at the parser's script boundary.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// One-based monotone boundary ordinal within this exact parser.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// Source span of the explicit closing script token.
    #[must_use]
    pub const fn closing_span(&self) -> SourceSpan {
        self.closing_span
    }

    /// Execution-affecting state captured at insertion of the start tag.
    #[must_use]
    pub const fn start_tag(&self) -> &ParserScriptStartTag {
        &self.start_tag
    }
}

#[derive(Debug)]
pub enum ParserStateError {
    AlreadyFinished,
    ScriptHandlerAborted,
    Tokenizer(TokenizerStateError),
    Dom(DomError),
}

impl fmt::Display for ParserStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyFinished => formatter.write_str("HTML parser has already finished"),
            Self::ScriptHandlerAborted => {
                formatter.write_str("HTML parser script handler did not complete normally")
            }
            Self::Tokenizer(error) => error.fmt(formatter),
            Self::Dom(error) => write!(formatter, "DOM construction failed: {error}"),
        }
    }
}

impl std::error::Error for ParserStateError {}

impl From<TokenizerStateError> for ParserStateError {
    fn from(value: TokenizerStateError) -> Self {
        Self::Tokenizer(value)
    }
}

impl From<DomError> for ParserStateError {
    fn from(value: DomError) -> Self {
        Self::Dom(value)
    }
}

/// Failure from script-aware token processing.
#[derive(Debug)]
pub enum ScriptHandlerError<E> {
    Parser(ParserStateError),
    Handler(E),
}

impl<E: fmt::Display> fmt::Display for ScriptHandlerError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parser(error) => error.fmt(formatter),
            Self::Handler(error) => {
                write!(formatter, "parser-inserted script handler failed: {error}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ScriptHandlerError<E> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertionMode {
    BeforeHtml,
    BeforeHead,
    InHead,
    AfterHead,
    InBody,
    AfterBody,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessAction {
    Consumed,
    Reprocess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParserLifecycle {
    Active,
    ScriptHandlerActive,
    ScriptHandlerAborted,
    Finished,
}

/// Incremental HTML tree builder for the static-page wave-one subset.
#[derive(Debug)]
pub struct HtmlParser {
    tokenizer: Tokenizer,
    document: Document,
    errors: Vec<ParseError>,
    insertion_mode: InsertionMode,
    open_elements: Vec<NodeId>,
    html_element: Option<NodeId>,
    head_element: Option<NodeId>,
    body_element: Option<NodeId>,
    document_mode: DocumentMode,
    drop_next_lf: bool,
    max_tree_depth: usize,
    current_token_position: SourcePosition,
    completed_script_boundaries: u64,
    current_script: Option<PendingParserScript>,
    completed_script: Option<ParserInsertedScript>,
    lifecycle: ParserLifecycle,
}

impl Default for HtmlParser {
    fn default() -> Self {
        Self::new(TokenizerLimits::default())
    }
}

impl HtmlParser {
    #[must_use]
    pub fn new(limits: TokenizerLimits) -> Self {
        Self::from_validated_document(limits, Document::new())
    }

    /// Starts parsing in an exact caller-owned pristine document arena.
    ///
    /// This ownership seam lets the browser keep one document identity while
    /// alternately lending it to the parser and a rooted script host.
    ///
    /// # Errors
    ///
    /// Returns a DOM invariant error unless `document` is an untouched arena
    /// with only its document node and revision zero.
    pub fn from_pristine_document(
        limits: TokenizerLimits,
        document: Document,
    ) -> Result<Self, ParserStateError> {
        if document.revision() != 0 || !document.children(document.document_node())?.is_empty() {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "HTML parser requires a pristine caller-owned document",
            )));
        }
        Ok(Self::from_validated_document(limits, document))
    }

    fn from_validated_document(limits: TokenizerLimits, document: Document) -> Self {
        let max_tree_depth = limits.max_tree_depth.max(2);
        Self {
            tokenizer: Tokenizer::new(limits),
            document,
            errors: Vec::new(),
            insertion_mode: InsertionMode::BeforeHtml,
            open_elements: Vec::new(),
            html_element: None,
            head_element: None,
            body_element: None,
            document_mode: DocumentMode::NoQuirks,
            drop_next_lf: false,
            max_tree_depth,
            current_token_position: SourcePosition::default(),
            completed_script_boundaries: 0,
            current_script: None,
            completed_script: None,
            lifecycle: ParserLifecycle::Active,
        }
    }

    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    #[must_use]
    pub fn errors(&self) -> &[ParseError] {
        &self.errors
    }

    /// # Errors
    ///
    /// Returns a tokenizer, tree-construction, or lifecycle error.
    pub fn feed(&mut self, input: &str) -> Result<(), ParserStateError> {
        let mut ignore = |_: &mut Document, _: ParserInsertedScript| Ok::<(), Infallible>(());
        match self.feed_with_script_handler(input, &mut ignore) {
            Ok(()) => Ok(()),
            Err(ScriptHandlerError::Parser(error)) => Err(error),
            Err(ScriptHandlerError::Handler(never)) => match never {},
        }
    }

    /// # Errors
    ///
    /// Returns a tokenizer, tree-construction, or lifecycle error.
    pub fn finish(self) -> Result<ParseOutput, ParserStateError> {
        let mut ignore = |_: &mut Document, _: ParserInsertedScript| Ok::<(), Infallible>(());
        match self.finish_with_script_handler(&mut ignore) {
            Ok(output) => Ok(output),
            Err(ScriptHandlerError::Parser(error)) => Err(error),
            Err(ScriptHandlerError::Handler(never)) => match never {},
        }
    }

    /// Feed input and synchronously stop at every completed parser-inserted
    /// script before processing the following token.
    ///
    /// The handler receives the same mutable document arena owned by this
    /// parser. Returning `Ok(())` resumes token processing. Returning an error,
    /// or unwinding, permanently closes this parser to later input.
    ///
    /// # Errors
    ///
    /// Returns [`ScriptHandlerError::Parser`] for tokenizer, tree, or lifecycle
    /// failures and [`ScriptHandlerError::Handler`] for the exact
    /// error returned by `handler`.
    pub fn feed_with_script_handler<E>(
        &mut self,
        input: &str,
        handler: &mut impl FnMut(&mut Document, ParserInsertedScript) -> Result<(), E>,
    ) -> Result<(), ScriptHandlerError<E>> {
        self.ensure_active().map_err(ScriptHandlerError::Parser)?;
        let tokens = self
            .tokenizer
            .feed(input)
            .map_err(ParserStateError::Tokenizer)
            .map_err(ScriptHandlerError::Parser)?;
        self.errors.extend(self.tokenizer.take_errors());
        self.process_tokens_with_script_handler(tokens, handler)
    }

    /// Finish tokenization while preserving the same exact script-boundary
    /// callback ordering as [`Self::feed_with_script_handler`].
    ///
    /// # Errors
    ///
    /// Returns [`ScriptHandlerError::Parser`] for tokenizer, tree, or lifecycle
    /// failures and [`ScriptHandlerError::Handler`] for the exact
    /// error returned by `handler`.
    pub fn finish_with_script_handler<E>(
        mut self,
        handler: &mut impl FnMut(&mut Document, ParserInsertedScript) -> Result<(), E>,
    ) -> Result<ParseOutput, ScriptHandlerError<E>> {
        self.ensure_active().map_err(ScriptHandlerError::Parser)?;
        let tokens = self
            .tokenizer
            .finish()
            .map_err(ParserStateError::Tokenizer)
            .map_err(ScriptHandlerError::Parser)?;
        self.errors.extend(self.tokenizer.take_errors());
        self.process_tokens_with_script_handler(tokens, handler)?;
        if self.current_name() == Some("script") {
            self.abandon_current_parser_script_at_eof()
                .map_err(ScriptHandlerError::Parser)?;
        } else if self.current_script.is_some() {
            return Err(ScriptHandlerError::Parser(ParserStateError::Dom(
                DomError::SnapshotInvariant(
                    "parser retained start-tag script state without an open script",
                ),
            )));
        }
        self.ensure_final_structure()
            .map_err(ScriptHandlerError::Parser)?;
        self.lifecycle = ParserLifecycle::Finished;
        self.document
            .validate_invariants()
            .map_err(ParserStateError::Dom)
            .map_err(ScriptHandlerError::Parser)?;
        let completion = ParserCompletionProof {
            document_version: self.document.version(),
            script_boundaries: self.completed_script_boundaries,
        };
        Ok(ParseOutput {
            document: self.document,
            errors: self.errors,
            document_mode: self.document_mode,
            completion,
        })
    }

    fn ensure_active(&self) -> Result<(), ParserStateError> {
        match self.lifecycle {
            ParserLifecycle::Active => Ok(()),
            ParserLifecycle::Finished => Err(ParserStateError::AlreadyFinished),
            ParserLifecycle::ScriptHandlerActive | ParserLifecycle::ScriptHandlerAborted => {
                Err(ParserStateError::ScriptHandlerAborted)
            }
        }
    }

    fn process_tokens_with_script_handler<E>(
        &mut self,
        tokens: Vec<SpannedToken>,
        handler: &mut impl FnMut(&mut Document, ParserInsertedScript) -> Result<(), E>,
    ) -> Result<(), ScriptHandlerError<E>> {
        for token in tokens {
            let SpannedToken { token, span } = token;
            if let Token::Character(data) = token {
                self.process_character_runs(&data, span)
                    .map_err(ScriptHandlerError::Parser)?;
            } else {
                self.process_token(SpannedToken { token, span })
                    .map_err(ScriptHandlerError::Parser)?;
            }
            self.dispatch_completed_script(handler)?;
        }
        Ok(())
    }

    fn dispatch_completed_script<E>(
        &mut self,
        handler: &mut impl FnMut(&mut Document, ParserInsertedScript) -> Result<(), E>,
    ) -> Result<(), ScriptHandlerError<E>> {
        let Some(script) = self.completed_script.take() else {
            return Ok(());
        };
        let ordinal = script.ordinal();
        self.lifecycle = ParserLifecycle::ScriptHandlerActive;
        let result = handler(&mut self.document, script);
        self.lifecycle = ParserLifecycle::Active;
        if let Err(error) = result {
            self.lifecycle = ParserLifecycle::ScriptHandlerAborted;
            return Err(ScriptHandlerError::Handler(error));
        }
        if ordinal
            != self
                .completed_script_boundaries
                .checked_add(1)
                .ok_or_else(|| {
                    ScriptHandlerError::Parser(ParserStateError::Dom(DomError::SnapshotInvariant(
                        "parser script boundary ordinal overflow",
                    )))
                })?
        {
            self.lifecycle = ParserLifecycle::ScriptHandlerAborted;
            return Err(ScriptHandlerError::Parser(ParserStateError::Dom(
                DomError::SnapshotInvariant("parser script boundary ordinal drifted"),
            )));
        }
        self.completed_script_boundaries = ordinal;
        Ok(())
    }

    fn process_character_runs(
        &mut self,
        data: &str,
        span: SourceSpan,
    ) -> Result<(), ParserStateError> {
        let mut run_start = 0;
        let mut run_is_space = None;
        for (offset, character) in data.char_indices() {
            let is_space = is_html_space(character);
            if run_is_space.is_some_and(|previous| previous != is_space) {
                self.process_token(SpannedToken {
                    token: Token::Character(data[run_start..offset].to_owned()),
                    span,
                })?;
                run_start = offset;
            }
            run_is_space = Some(is_space);
        }
        if run_start < data.len() {
            self.process_token(SpannedToken {
                token: Token::Character(data[run_start..].to_owned()),
                span,
            })?;
        }
        Ok(())
    }

    fn process_token(&mut self, mut token: SpannedToken) -> Result<(), ParserStateError> {
        self.current_token_position = token.span.start;
        if self.drop_next_lf {
            self.drop_next_lf = false;
            if let Token::Character(data) = &mut token.token
                && data.starts_with('\n')
            {
                data.remove(0);
                if data.is_empty() {
                    return Ok(());
                }
            }
        }
        for _ in 0..8 {
            if self.process_once(&token)? == ProcessAction::Consumed {
                return Ok(());
            }
        }
        Err(ParserStateError::Dom(DomError::SnapshotInvariant(
            "tree-builder reprocessing did not converge",
        )))
    }

    fn process_once(&mut self, token: &SpannedToken) -> Result<ProcessAction, ParserStateError> {
        match &token.token {
            Token::Doctype {
                name,
                public_id,
                system_id,
                force_quirks,
            } => {
                if self.insertion_mode == InsertionMode::BeforeHtml
                    && self.document.doctype().is_none()
                    && self.html_element.is_none()
                {
                    let doctype = self.document.create_doctype(
                        name.clone(),
                        public_id.clone(),
                        system_id.clone(),
                    )?;
                    self.document
                        .append_child(self.document.document_node(), doctype)?;
                    if *force_quirks || !name.eq_ignore_ascii_case("html") {
                        self.document_mode = DocumentMode::Quirks;
                    }
                } else {
                    self.error(ParseErrorCode::UnexpectedDoctype, token.span.start);
                }
                Ok(ProcessAction::Consumed)
            }
            Token::Comment(data) => {
                self.insert_comment(data)?;
                Ok(ProcessAction::Consumed)
            }
            Token::Character(data) => self.process_characters(data, token.span),
            Token::StartTag {
                name,
                attributes,
                self_closing,
            } => self.process_start_tag(name, attributes, *self_closing, token.span),
            Token::EndTag { name } => self.process_end_tag(name, token.span),
        }
    }

    fn process_characters(
        &mut self,
        data: &str,
        span: SourceSpan,
    ) -> Result<ProcessAction, ParserStateError> {
        match self.insertion_mode {
            InsertionMode::BeforeHtml => {
                if data.chars().all(is_html_space) {
                    return Ok(ProcessAction::Consumed);
                }
                self.error(
                    ParseErrorCode::UnexpectedCharactersBeforeDocumentElement,
                    span.start,
                );
                self.ensure_html(&[])?;
                self.insertion_mode = InsertionMode::BeforeHead;
                Ok(ProcessAction::Reprocess)
            }
            InsertionMode::BeforeHead => {
                if data.chars().all(is_html_space) {
                    return Ok(ProcessAction::Consumed);
                }
                self.ensure_head(&[])?;
                self.insertion_mode = InsertionMode::InHead;
                Ok(ProcessAction::Reprocess)
            }
            InsertionMode::InHead => {
                if data.chars().all(is_html_space)
                    || self.current_name().is_some_and(is_raw_text_element)
                {
                    self.append_text_to_current(data)?;
                    Ok(ProcessAction::Consumed)
                } else {
                    self.error(ParseErrorCode::UnexpectedCharactersInHead, span.start);
                    self.close_head();
                    self.insertion_mode = InsertionMode::AfterHead;
                    Ok(ProcessAction::Reprocess)
                }
            }
            InsertionMode::AfterHead => {
                self.ensure_body(&[])?;
                self.insertion_mode = InsertionMode::InBody;
                Ok(ProcessAction::Reprocess)
            }
            InsertionMode::InBody => {
                self.append_text_to_current(data)?;
                Ok(ProcessAction::Consumed)
            }
            InsertionMode::AfterBody => {
                if data.chars().all(is_html_space) {
                    let body = self.ensure_body(&[])?;
                    self.document.append_text(body, data)?;
                    Ok(ProcessAction::Consumed)
                } else {
                    self.error(ParseErrorCode::UnexpectedCharactersInHead, span.start);
                    self.insertion_mode = InsertionMode::InBody;
                    Ok(ProcessAction::Reprocess)
                }
            }
        }
    }

    fn process_start_tag(
        &mut self,
        name: &str,
        attributes: &[SpannedAttribute],
        self_closing: bool,
        span: SourceSpan,
    ) -> Result<ProcessAction, ParserStateError> {
        match self.insertion_mode {
            InsertionMode::BeforeHtml => {
                if name == "html" {
                    self.ensure_html(attributes)?;
                    self.insertion_mode = InsertionMode::BeforeHead;
                    Ok(ProcessAction::Consumed)
                } else {
                    self.ensure_html(&[])?;
                    self.insertion_mode = InsertionMode::BeforeHead;
                    Ok(ProcessAction::Reprocess)
                }
            }
            InsertionMode::BeforeHead => match name {
                "html" => {
                    let html = self.ensure_html(&[])?;
                    self.merge_attributes(html, attributes)?;
                    Ok(ProcessAction::Consumed)
                }
                "head" => {
                    self.ensure_head(attributes)?;
                    self.insertion_mode = InsertionMode::InHead;
                    Ok(ProcessAction::Consumed)
                }
                _ => {
                    self.ensure_head(&[])?;
                    self.insertion_mode = InsertionMode::InHead;
                    Ok(ProcessAction::Reprocess)
                }
            },
            InsertionMode::InHead => match name {
                "html" => {
                    let html = self.ensure_html(&[])?;
                    self.merge_attributes(html, attributes)?;
                    Ok(ProcessAction::Consumed)
                }
                "head" => {
                    self.error(ParseErrorCode::UnexpectedStartTag, span.start);
                    Ok(ProcessAction::Consumed)
                }
                "base" | "basefont" | "bgsound" | "link" | "meta" => {
                    self.insert_element(name, attributes, false)?;
                    Ok(ProcessAction::Consumed)
                }
                "title" | "style" | "script" | "noframes" => {
                    let element = self.insert_element(name, attributes, true)?;
                    if name == "script" {
                        self.begin_parser_script(element, attributes, span)?;
                    }
                    Ok(ProcessAction::Consumed)
                }
                "body" => {
                    self.close_head();
                    self.insertion_mode = InsertionMode::AfterHead;
                    Ok(ProcessAction::Reprocess)
                }
                _ => {
                    self.close_head();
                    self.insertion_mode = InsertionMode::AfterHead;
                    Ok(ProcessAction::Reprocess)
                }
            },
            InsertionMode::AfterHead => match name {
                "html" => {
                    let html = self.ensure_html(&[])?;
                    self.merge_attributes(html, attributes)?;
                    Ok(ProcessAction::Consumed)
                }
                "body" => {
                    self.ensure_body(attributes)?;
                    self.insertion_mode = InsertionMode::InBody;
                    Ok(ProcessAction::Consumed)
                }
                "head" => {
                    self.error(ParseErrorCode::UnexpectedStartTag, span.start);
                    Ok(ProcessAction::Consumed)
                }
                "base" | "basefont" | "bgsound" | "link" | "meta" => {
                    self.error(ParseErrorCode::UnexpectedStartTag, span.start);
                    let head = self.ensure_head(&[])?;
                    self.append_element_to(head, name, attributes, false)?;
                    Ok(ProcessAction::Consumed)
                }
                "title" | "style" | "script" | "noframes" => {
                    self.error(ParseErrorCode::UnexpectedStartTag, span.start);
                    let head = self.ensure_head(&[])?;
                    if self.open_elements.last().copied() != Some(head) {
                        self.open_elements.push(head);
                    }
                    let element = self.insert_element(name, attributes, true)?;
                    if name == "script" {
                        self.begin_parser_script(element, attributes, span)?;
                    }
                    self.insertion_mode = InsertionMode::InHead;
                    Ok(ProcessAction::Consumed)
                }
                _ => {
                    self.ensure_body(&[])?;
                    self.insertion_mode = InsertionMode::InBody;
                    Ok(ProcessAction::Reprocess)
                }
            },
            InsertionMode::InBody => {
                if name == "html" {
                    let html = self.ensure_html(&[])?;
                    self.merge_attributes(html, attributes)?;
                    return Ok(ProcessAction::Consumed);
                }
                if name == "body" {
                    let body = self.ensure_body(&[])?;
                    self.merge_attributes(body, attributes)?;
                    return Ok(ProcessAction::Consumed);
                }
                if name == "head" {
                    self.error(ParseErrorCode::UnexpectedStartTag, span.start);
                    return Ok(ProcessAction::Consumed);
                }

                if name == "p" {
                    self.close_if_in_scope("p", span.start)?;
                } else if name == "li" {
                    self.close_if_in_scope("li", span.start)?;
                    self.close_if_in_scope("p", span.start)?;
                } else if is_heading(name) {
                    self.close_if_in_scope("p", span.start)?;
                    if self.current_name().is_some_and(is_heading) {
                        self.error(ParseErrorCode::MismatchedEndTag, span.start);
                        self.open_elements.pop();
                    }
                } else if is_block_element(name) {
                    self.close_if_in_scope("p", span.start)?;
                }

                let is_void = is_void_element(name);
                if self_closing && !is_void {
                    self.error(
                        ParseErrorCode::NonVoidHtmlElementStartTagWithTrailingSolidus,
                        span.start,
                    );
                }
                let element = self.insert_element(name, attributes, !is_void)?;
                if name == "script" {
                    self.begin_parser_script(element, attributes, span)?;
                }
                if matches!(name, "pre" | "listing" | "textarea") {
                    self.drop_next_lf = true;
                }
                Ok(ProcessAction::Consumed)
            }
            InsertionMode::AfterBody => match name {
                "html" => {
                    let html = self.ensure_html(&[])?;
                    self.merge_attributes(html, attributes)?;
                    Ok(ProcessAction::Consumed)
                }
                _ => {
                    self.error(ParseErrorCode::UnexpectedStartTag, span.start);
                    self.insertion_mode = InsertionMode::InBody;
                    Ok(ProcessAction::Reprocess)
                }
            },
        }
    }

    fn process_end_tag(
        &mut self,
        name: &str,
        span: SourceSpan,
    ) -> Result<ProcessAction, ParserStateError> {
        match self.insertion_mode {
            InsertionMode::BeforeHtml | InsertionMode::BeforeHead => {
                if matches!(name, "head" | "body" | "html" | "br") {
                    if self.insertion_mode == InsertionMode::BeforeHtml {
                        self.ensure_html(&[])?;
                        self.insertion_mode = InsertionMode::BeforeHead;
                    } else {
                        self.ensure_head(&[])?;
                        self.insertion_mode = InsertionMode::InHead;
                    }
                    Ok(ProcessAction::Reprocess)
                } else {
                    self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    Ok(ProcessAction::Consumed)
                }
            }
            InsertionMode::InHead => {
                if name == "head" {
                    self.close_head();
                    self.insertion_mode = InsertionMode::AfterHead;
                    return Ok(ProcessAction::Consumed);
                }
                if self.current_name() == Some(name) && is_raw_text_element(name) {
                    if name == "script" {
                        self.complete_current_parser_script(span)?;
                    } else {
                        self.open_elements.pop();
                    }
                    return Ok(ProcessAction::Consumed);
                }
                if matches!(name, "body" | "html" | "br") {
                    self.close_head();
                    self.insertion_mode = InsertionMode::AfterHead;
                    return Ok(ProcessAction::Reprocess);
                }
                self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                Ok(ProcessAction::Consumed)
            }
            InsertionMode::AfterHead => {
                if matches!(name, "body" | "html" | "br") {
                    self.ensure_body(&[])?;
                    self.insertion_mode = InsertionMode::InBody;
                    Ok(ProcessAction::Reprocess)
                } else {
                    self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    Ok(ProcessAction::Consumed)
                }
            }
            InsertionMode::InBody => {
                if name == "body" {
                    if self
                        .body_element
                        .is_some_and(|body| self.open_elements.contains(&body))
                    {
                        self.insertion_mode = InsertionMode::AfterBody;
                    } else {
                        self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    }
                    return Ok(ProcessAction::Consumed);
                }
                if name == "html" {
                    if self.body_element.is_some() {
                        self.insertion_mode = InsertionMode::AfterBody;
                    } else {
                        self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    }
                    return Ok(ProcessAction::Consumed);
                }
                if name == "script" && self.current_name() == Some("script") {
                    self.complete_current_parser_script(span)?;
                    return Ok(ProcessAction::Consumed);
                }
                if name == "p" && !self.has_in_scope("p")? {
                    self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    let paragraph = self.insert_element("p", &[], true)?;
                    debug_assert_eq!(self.open_elements.last().copied(), Some(paragraph));
                }
                if name == "br" {
                    self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    self.insert_element("br", &[], false)?;
                    return Ok(ProcessAction::Consumed);
                }
                if self.has_in_scope(name)? && self.current_name() != Some(name) {
                    self.error(ParseErrorCode::MismatchedEndTag, span.start);
                }
                if self.close_named(name)? {
                    Ok(ProcessAction::Consumed)
                } else {
                    self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    Ok(ProcessAction::Consumed)
                }
            }
            InsertionMode::AfterBody => {
                if name == "html" {
                    Ok(ProcessAction::Consumed)
                } else {
                    self.error(ParseErrorCode::UnexpectedEndTag, span.start);
                    self.insertion_mode = InsertionMode::InBody;
                    Ok(ProcessAction::Reprocess)
                }
            }
        }
    }

    fn insert_comment(&mut self, data: &str) -> Result<(), ParserStateError> {
        let parent = match self.insertion_mode {
            InsertionMode::BeforeHtml => self.document.document_node(),
            InsertionMode::AfterBody => self
                .html_element
                .unwrap_or_else(|| self.document.document_node()),
            _ => self.current_node(),
        };
        let comment = self.document.create_comment(data)?;
        self.document.append_child(parent, comment)?;
        Ok(())
    }

    fn ensure_html(&mut self, attributes: &[SpannedAttribute]) -> Result<NodeId, ParserStateError> {
        if let Some(html) = self.html_element {
            self.merge_attributes(html, attributes)?;
            return Ok(html);
        }
        if self.document.doctype().is_none() {
            self.document_mode = DocumentMode::Quirks;
        }
        let html = self.document.create_html_element("html")?;
        self.apply_attributes(html, attributes)?;
        self.document
            .append_child(self.document.document_node(), html)?;
        self.html_element = Some(html);
        self.open_elements.clear();
        self.open_elements.push(html);
        Ok(html)
    }

    fn ensure_head(&mut self, attributes: &[SpannedAttribute]) -> Result<NodeId, ParserStateError> {
        if let Some(head) = self.head_element {
            self.merge_attributes(head, attributes)?;
            if self.open_elements.last().copied() != Some(head)
                && self.insertion_mode == InsertionMode::BeforeHead
            {
                self.open_elements.push(head);
            }
            return Ok(head);
        }
        let html = self.ensure_html(&[])?;
        let head = self.append_element_to(html, "head", attributes, false)?;
        self.head_element = Some(head);
        self.open_elements.truncate(1);
        self.open_elements.push(head);
        Ok(head)
    }

    fn ensure_body(&mut self, attributes: &[SpannedAttribute]) -> Result<NodeId, ParserStateError> {
        if let Some(body) = self.body_element {
            self.merge_attributes(body, attributes)?;
            if !self.open_elements.contains(&body) {
                self.open_elements.truncate(1);
                self.open_elements.push(body);
            }
            return Ok(body);
        }
        let html = self.ensure_html(&[])?;
        if self.head_element.is_none() {
            self.ensure_head(&[])?;
            self.close_head();
        }
        let body = self.append_element_to(html, "body", attributes, false)?;
        self.body_element = Some(body);
        self.open_elements.truncate(1);
        self.open_elements.push(body);
        Ok(body)
    }

    fn ensure_final_structure(&mut self) -> Result<(), ParserStateError> {
        self.ensure_html(&[])?;
        if self.head_element.is_none() {
            self.ensure_head(&[])?;
            self.close_head();
        }
        self.ensure_body(&[])?;
        Ok(())
    }

    fn close_head(&mut self) {
        if let Some(head) = self.head_element
            && let Some(position) = self.open_elements.iter().position(|node| *node == head)
        {
            self.open_elements.truncate(position);
        }
        if self.open_elements.is_empty()
            && let Some(html) = self.html_element
        {
            self.open_elements.push(html);
        }
    }

    fn insert_element(
        &mut self,
        name: &str,
        attributes: &[SpannedAttribute],
        push: bool,
    ) -> Result<NodeId, ParserStateError> {
        let parent = self.current_node();
        let element = self.append_element_to(parent, name, attributes, false)?;
        if push {
            if self.open_elements.len() < self.max_tree_depth {
                self.open_elements.push(element);
            } else {
                self.error(
                    ParseErrorCode::TreeDepthLimitExceeded,
                    self.current_token_position,
                );
            }
        }
        Ok(element)
    }

    fn append_element_to(
        &mut self,
        parent: NodeId,
        name: &str,
        attributes: &[SpannedAttribute],
        _push: bool,
    ) -> Result<NodeId, ParserStateError> {
        let element = self.document.create_html_element(name)?;
        self.apply_attributes(element, attributes)?;
        self.document.append_child(parent, element)?;
        Ok(element)
    }

    fn apply_attributes(
        &mut self,
        element: NodeId,
        attributes: &[SpannedAttribute],
    ) -> Result<(), ParserStateError> {
        for attribute in attributes {
            self.document.set_attribute(
                element,
                AttributeName::html(&attribute.name),
                attribute.value.clone(),
            )?;
        }
        Ok(())
    }

    fn merge_attributes(
        &mut self,
        element: NodeId,
        attributes: &[SpannedAttribute],
    ) -> Result<(), ParserStateError> {
        for attribute in attributes {
            if self
                .document
                .attribute(element, None, &attribute.name)?
                .is_none()
            {
                self.document.set_attribute(
                    element,
                    AttributeName::html(&attribute.name),
                    attribute.value.clone(),
                )?;
            }
        }
        Ok(())
    }

    fn append_text_to_current(&mut self, data: &str) -> Result<(), ParserStateError> {
        let parent = self.current_node();
        self.document.append_text(parent, data)?;
        Ok(())
    }

    fn complete_current_parser_script(
        &mut self,
        closing_span: SourceSpan,
    ) -> Result<(), ParserStateError> {
        if self.completed_script.is_some() {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "parser completed two scripts without dispatching the first",
            )));
        }
        let node = self
            .open_elements
            .last()
            .copied()
            .ok_or(ParserStateError::Dom(DomError::SnapshotInvariant(
                "parser script has no current element",
            )))?;
        let NodeKind::Element(element) = self.document.node_kind(node)? else {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "parser script boundary does not name an element",
            )));
        };
        if element.name.local_name != "script" {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "parser script boundary names a non-script element",
            )));
        }
        let pending = self.current_script.take().ok_or(ParserStateError::Dom(
            DomError::SnapshotInvariant("parser script boundary has no start-tag execution state"),
        ))?;
        if pending.node != node {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "parser script start and end tags name different nodes",
            )));
        }
        let ordinal =
            self.completed_script_boundaries
                .checked_add(1)
                .ok_or(ParserStateError::Dom(DomError::SnapshotInvariant(
                    "parser script boundary ordinal overflow",
                )))?;
        let candidate = ParserInsertedScript {
            node,
            document_version: self.document.version(),
            ordinal,
            closing_span,
            start_tag: pending.start_tag,
        };
        self.open_elements.pop();
        self.completed_script = Some(candidate);
        Ok(())
    }

    fn begin_parser_script(
        &mut self,
        node: NodeId,
        attributes: &[SpannedAttribute],
        opening_span: SourceSpan,
    ) -> Result<(), ParserStateError> {
        if self.current_script.is_some() || self.completed_script.is_some() {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "parser inserted a script while another script boundary was pending",
            )));
        }
        let base_href = self.current_first_base_href()?;
        self.current_script = Some(PendingParserScript {
            node,
            start_tag: ParserScriptStartTag {
                opening_span,
                base_href,
                src: start_tag_attribute(attributes, "src"),
                script_type: start_tag_attribute(attributes, "type"),
                language: start_tag_attribute(attributes, "language"),
                charset: start_tag_attribute(attributes, "charset"),
                cross_origin: start_tag_attribute(attributes, "crossorigin"),
                integrity: start_tag_attribute(attributes, "integrity"),
                nonce: start_tag_attribute(attributes, "nonce"),
                referrer_policy: start_tag_attribute(attributes, "referrerpolicy"),
                fetch_priority: start_tag_attribute(attributes, "fetchpriority"),
                blocking: start_tag_attribute(attributes, "blocking"),
                async_present: has_start_tag_attribute(attributes, "async"),
                defer_present: has_start_tag_attribute(attributes, "defer"),
                no_module_present: has_start_tag_attribute(attributes, "nomodule"),
            },
        });
        Ok(())
    }

    fn abandon_current_parser_script_at_eof(&mut self) -> Result<(), ParserStateError> {
        let node = self
            .open_elements
            .last()
            .copied()
            .ok_or(ParserStateError::Dom(DomError::SnapshotInvariant(
                "EOF script has no current element",
            )))?;
        let pending = self.current_script.take().ok_or(ParserStateError::Dom(
            DomError::SnapshotInvariant("EOF script has no start-tag execution state"),
        ))?;
        if pending.node != node {
            return Err(ParserStateError::Dom(DomError::SnapshotInvariant(
                "EOF script start state names a different node",
            )));
        }
        self.open_elements.pop();
        self.error(ParseErrorCode::EofInScript, self.tokenizer.position());
        Ok(())
    }

    fn current_first_base_href(&self) -> Result<Option<String>, ParserStateError> {
        for base in self.document.elements_by_tag_name("base")? {
            if let Some(href) = self.document.attribute(base, None, "href")? {
                return Ok(Some(href.to_owned()));
            }
        }
        Ok(None)
    }

    fn current_node(&self) -> NodeId {
        self.open_elements
            .last()
            .copied()
            .unwrap_or_else(|| self.document.document_node())
    }

    fn current_name(&self) -> Option<&str> {
        let current = self.open_elements.last().copied()?;
        let NodeKind::Element(element) = self.document.node_kind(current).ok()? else {
            return None;
        };
        Some(element.name.local_name.as_str())
    }

    fn node_name(&self, node: NodeId) -> Result<Option<&str>, ParserStateError> {
        Ok(match self.document.node_kind(node)? {
            NodeKind::Element(element) => Some(element.name.local_name.as_str()),
            _ => None,
        })
    }

    fn has_in_scope(&self, name: &str) -> Result<bool, ParserStateError> {
        for node in self.open_elements.iter().rev() {
            let Some(candidate) = self.node_name(*node)? else {
                continue;
            };
            if candidate == name {
                return Ok(true);
            }
            if matches!(candidate, "html" | "table" | "template") {
                break;
            }
        }
        Ok(false)
    }

    fn close_if_in_scope(
        &mut self,
        name: &str,
        position: SourcePosition,
    ) -> Result<bool, ParserStateError> {
        if !self.has_in_scope(name)? {
            return Ok(false);
        }
        if self.current_name() != Some(name) {
            self.error(ParseErrorCode::MismatchedEndTag, position);
        }
        self.close_named(name)
    }

    fn close_named(&mut self, name: &str) -> Result<bool, ParserStateError> {
        let mut match_position = None;
        for (position, node) in self.open_elements.iter().enumerate().rev() {
            if self.node_name(*node)? == Some(name) {
                match_position = Some(position);
                break;
            }
            if self.node_name(*node)? == Some("html") {
                break;
            }
        }
        let Some(position) = match_position else {
            return Ok(false);
        };
        self.open_elements.truncate(position);
        Ok(true)
    }

    fn error(&mut self, code: ParseErrorCode, position: SourcePosition) {
        self.errors.push(ParseError {
            phase: ParsePhase::TreeBuilder,
            code,
            position,
        });
    }
}

fn start_tag_attribute(attributes: &[SpannedAttribute], name: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.value.clone())
}

fn has_start_tag_attribute(attributes: &[SpannedAttribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.name == name)
}

pub fn parse_document(source: &str) -> Result<ParseOutput, ParserStateError> {
    let mut parser = HtmlParser::default();
    parser.feed(source)?;
    parser.finish()
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// The five ASCII space characters used by the HTML parsing algorithm.
fn is_html_space(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{000c}' | '\r' | ' ')
}

fn is_raw_text_element(name: &str) -> bool {
    matches!(
        name,
        "title"
            | "textarea"
            | "script"
            | "style"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "plaintext"
    )
}

fn is_heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

fn is_block_element(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "header"
            | "hgroup"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "search"
            | "section"
            | "summary"
            | "ul"
            | "pre"
            | "listing"
            | "form"
            | "table"
    ) || is_heading(name)
}
