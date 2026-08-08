use std::fmt;

use wild_buzzard_dom::{AttributeName, Document, DomError, NodeId, NodeKind};

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
}

#[derive(Debug)]
pub enum ParserStateError {
    AlreadyFinished,
    Tokenizer(TokenizerStateError),
    Dom(DomError),
}

impl fmt::Display for ParserStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyFinished => formatter.write_str("HTML parser has already finished"),
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
    finished: bool,
}

impl Default for HtmlParser {
    fn default() -> Self {
        Self::new(TokenizerLimits::default())
    }
}

impl HtmlParser {
    pub fn new(limits: TokenizerLimits) -> Self {
        let max_tree_depth = limits.max_tree_depth.max(2);
        Self {
            tokenizer: Tokenizer::new(limits),
            document: Document::new(),
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
            finished: false,
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn errors(&self) -> &[ParseError] {
        &self.errors
    }

    pub fn feed(&mut self, input: &str) -> Result<(), ParserStateError> {
        if self.finished {
            return Err(ParserStateError::AlreadyFinished);
        }
        let tokens = self.tokenizer.feed(input)?;
        self.errors.extend(self.tokenizer.take_errors());
        self.process_tokens(tokens)
    }

    pub fn finish(mut self) -> Result<ParseOutput, ParserStateError> {
        if self.finished {
            return Err(ParserStateError::AlreadyFinished);
        }
        let tokens = self.tokenizer.finish()?;
        self.errors.extend(self.tokenizer.take_errors());
        self.process_tokens(tokens)?;
        self.ensure_final_structure()?;
        self.finished = true;
        self.document.validate_invariants()?;
        Ok(ParseOutput {
            document: self.document,
            errors: self.errors,
            document_mode: self.document_mode,
        })
    }

    fn process_tokens(&mut self, tokens: Vec<SpannedToken>) -> Result<(), ParserStateError> {
        for token in tokens {
            let SpannedToken { token, span } = token;
            if let Token::Character(data) = token {
                self.process_character_runs(&data, span)?;
            } else {
                self.process_token(SpannedToken { token, span })?;
            }
        }
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
                    self.insert_element(name, attributes, true)?;
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
                    self.insert_element(name, attributes, true)?;
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
                self.insert_element(name, attributes, !is_void)?;
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
                    self.open_elements.pop();
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
