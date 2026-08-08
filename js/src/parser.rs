use std::collections::HashSet;

use crate::ast::{
    AssignmentTarget, BinaryOperator, BindingDeclaration, CatchClause, DeclarationKind, Expression,
    ExpressionKind, Function, Literal, LogicalOperator, MemberProperty, ObjectProperty, Program,
    Statement, StatementKind, UnaryOperator,
};
use crate::error::SyntaxIssue;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::source::SourceSpan;

pub(crate) fn parse(source: &str) -> Result<Program, SyntaxIssue> {
    let tokens = Lexer::new(source).tokenize()?;
    Parser::new(tokens).parse_program()
}

struct Parser {
    tokens: Vec<Token>,
    current: usize,
    function_depth: usize,
    loop_depth: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            current: 0,
            function_depth: 0,
            loop_depth: 0,
        }
    }

    fn parse_program(mut self) -> Result<Program, SyntaxIssue> {
        let statements = self.parse_statement_list(false)?;
        validate_declarations(&statements, &HashSet::new())?;
        Ok(Program { statements })
    }

    fn parse_statement_list(&mut self, stop_at_brace: bool) -> Result<Vec<Statement>, SyntaxIssue> {
        let mut statements = Vec::new();
        while !(self.at(&TokenKind::Eof) || stop_at_brace && self.at(&TokenKind::RightBrace)) {
            statements.push(self.parse_statement_list_item()?);
        }
        if stop_at_brace && self.at(&TokenKind::Eof) {
            return Err(self.error_here("expected '}' before end of input"));
        }
        Ok(statements)
    }

    fn parse_statement_list_item(&mut self) -> Result<Statement, SyntaxIssue> {
        if self.at(&TokenKind::Let) || self.at(&TokenKind::Const) {
            self.parse_lexical_declaration()
        } else if self.at(&TokenKind::Function) {
            self.parse_function_declaration()
        } else {
            self.parse_statement()
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, SyntaxIssue> {
        if self.take(&TokenKind::Semicolon).is_some() {
            let span = self.previous().span;
            return Ok(Statement {
                kind: StatementKind::Empty,
                span,
            });
        }
        if self.at(&TokenKind::LeftBrace) {
            return self.parse_block();
        }
        if self.at(&TokenKind::If) {
            return self.parse_if();
        }
        if self.at(&TokenKind::While) {
            return self.parse_while();
        }
        if self.at(&TokenKind::Return) {
            return self.parse_return();
        }
        if self.at(&TokenKind::Throw) {
            return self.parse_throw();
        }
        if self.at(&TokenKind::Try) {
            return self.parse_try();
        }
        if self.at(&TokenKind::Break) {
            return self.parse_loop_control(true);
        }
        if self.at(&TokenKind::Continue) {
            return self.parse_loop_control(false);
        }
        if self.at(&TokenKind::Let) || self.at(&TokenKind::Const) {
            return Err(self.error_here("lexical declarations require a statement-list block"));
        }
        if self.at(&TokenKind::Function) {
            return Err(self.error_here("function declarations require a statement-list block"));
        }
        self.parse_expression_statement()
    }

    fn parse_block(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::LeftBrace, "expected '{'")?.span;
        let statements = self.parse_statement_list(true)?;
        validate_declarations(&statements, &HashSet::new())?;
        let end = self.expect(&TokenKind::RightBrace, "expected '}'")?.span;
        Ok(Statement {
            kind: StatementKind::Block(statements),
            span: start.join(end),
        })
    }

    fn parse_lexical_declaration(&mut self) -> Result<Statement, SyntaxIssue> {
        let keyword = self.advance().clone();
        let kind = if keyword.kind == TokenKind::Let {
            DeclarationKind::Let
        } else {
            DeclarationKind::Const
        };
        let mut bindings = Vec::new();
        loop {
            let name_token = self.advance().clone();
            let TokenKind::Identifier(name) = name_token.kind else {
                return Err(SyntaxIssue::new(
                    "expected a binding identifier",
                    name_token.span,
                ));
            };
            let initializer = if self.take(&TokenKind::Assign).is_some() {
                Some(self.parse_assignment()?)
            } else {
                None
            };
            if kind == DeclarationKind::Const && initializer.is_none() {
                return Err(SyntaxIssue::new(
                    "const declarations require an initializer",
                    name_token.span,
                ));
            }
            let end = initializer
                .as_ref()
                .map_or(name_token.span, |expression| expression.span);
            bindings.push(BindingDeclaration {
                name,
                initializer,
                span: name_token.span.join(end),
            });
            if self.take(&TokenKind::Comma).is_none() {
                break;
            }
        }
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: StatementKind::LexicalDeclaration { kind, bindings },
            span: keyword.span.join(end),
        })
    }

    fn parse_function_declaration(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self
            .expect(&TokenKind::Function, "expected 'function'")?
            .span;
        let function = self.parse_function_after_keyword(start, true)?;
        let span = function.span;
        Ok(Statement {
            kind: StatementKind::FunctionDeclaration(function),
            span,
        })
    }

    fn parse_function_after_keyword(
        &mut self,
        start: SourceSpan,
        name_required: bool,
    ) -> Result<Function, SyntaxIssue> {
        let name = match &self.peek().kind {
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Some(name)
            }
            _ if name_required => {
                return Err(self.error_here("function declaration requires a name"));
            }
            _ => None,
        };
        self.expect(&TokenKind::LeftParen, "expected '(' after function name")?;
        let mut parameters = Vec::new();
        let mut parameter_names = HashSet::new();
        if !self.at(&TokenKind::RightParen) {
            loop {
                let token = self.advance().clone();
                let TokenKind::Identifier(parameter) = token.kind else {
                    return Err(SyntaxIssue::new("expected a parameter name", token.span));
                };
                if !parameter_names.insert(parameter.clone()) {
                    return Err(SyntaxIssue::new(
                        format!("duplicate parameter '{parameter}' is not supported"),
                        token.span,
                    ));
                }
                parameters.push(parameter);
                if self.take(&TokenKind::Comma).is_none() {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RightParen, "expected ')' after parameters")?;
        self.expect(&TokenKind::LeftBrace, "expected '{' before function body")?;

        let outer_loop_depth = self.loop_depth;
        self.loop_depth = 0;
        self.function_depth += 1;
        let body_result = self.parse_statement_list(true);
        self.function_depth -= 1;
        self.loop_depth = outer_loop_depth;
        let body = body_result?;
        validate_declarations(&body, &parameter_names)?;
        let end = self
            .expect(&TokenKind::RightBrace, "expected '}' after function body")?
            .span;
        Ok(Function {
            name,
            parameters,
            body,
            span: start.join(end),
        })
    }

    fn parse_if(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::If, "expected 'if'")?.span;
        self.expect(&TokenKind::LeftParen, "expected '(' after 'if'")?;
        let test = self.parse_assignment()?;
        self.expect(&TokenKind::RightParen, "expected ')' after condition")?;
        let consequent = Box::new(self.parse_statement()?);
        let alternate = if self.take(&TokenKind::Else).is_some() {
            Some(Box::new(self.parse_statement()?))
        } else {
            None
        };
        let end = alternate
            .as_ref()
            .map_or(consequent.span, |statement| statement.span);
        Ok(Statement {
            kind: StatementKind::If {
                test,
                consequent,
                alternate,
            },
            span: start.join(end),
        })
    }

    fn parse_while(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::While, "expected 'while'")?.span;
        self.expect(&TokenKind::LeftParen, "expected '(' after 'while'")?;
        let test = self.parse_assignment()?;
        self.expect(&TokenKind::RightParen, "expected ')' after condition")?;
        self.loop_depth += 1;
        let body_result = self.parse_statement();
        self.loop_depth -= 1;
        let body = Box::new(body_result?);
        let span = start.join(body.span);
        Ok(Statement {
            kind: StatementKind::While { test, body },
            span,
        })
    }

    fn parse_return(&mut self) -> Result<Statement, SyntaxIssue> {
        let keyword = self
            .expect(&TokenKind::Return, "expected 'return'")?
            .clone();
        if self.function_depth == 0 {
            return Err(SyntaxIssue::new(
                "return is only valid inside a function",
                keyword.span,
            ));
        }
        let expression = if self.has_line_terminator_after(&keyword)
            || self.at(&TokenKind::Semicolon)
            || self.at(&TokenKind::RightBrace)
            || self.at(&TokenKind::Eof)
        {
            None
        } else {
            Some(self.parse_assignment()?)
        };
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: StatementKind::Return(expression),
            span: keyword.span.join(end),
        })
    }

    fn parse_throw(&mut self) -> Result<Statement, SyntaxIssue> {
        let keyword = self.expect(&TokenKind::Throw, "expected 'throw'")?.clone();
        if self.has_line_terminator_after(&keyword) {
            return Err(SyntaxIssue::new(
                "a line terminator is not allowed after 'throw'",
                keyword.span,
            ));
        }
        if self.at(&TokenKind::Semicolon)
            || self.at(&TokenKind::RightBrace)
            || self.at(&TokenKind::Eof)
        {
            return Err(self.error_here("throw requires an expression"));
        }
        let expression = self.parse_assignment()?;
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: StatementKind::Throw(expression),
            span: keyword.span.join(end),
        })
    }

    fn parse_try(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::Try, "expected 'try'")?.span;
        if !self.at(&TokenKind::LeftBrace) {
            return Err(self.error_here("try body must be a block"));
        }
        let body = Box::new(self.parse_block()?);
        let catch = if self.take(&TokenKind::Catch).is_some() {
            let catch_start = self.previous().span;
            let parameter = if self.take(&TokenKind::LeftParen).is_some() {
                let token = self.advance().clone();
                let TokenKind::Identifier(parameter) = token.kind else {
                    return Err(SyntaxIssue::new("expected a catch parameter", token.span));
                };
                self.expect(&TokenKind::RightParen, "expected ')' after catch parameter")?;
                Some(parameter)
            } else {
                None
            };
            if !self.at(&TokenKind::LeftBrace) {
                return Err(self.error_here("catch body must be a block"));
            }
            let catch_body = Box::new(self.parse_block()?);
            if let (Some(parameter), StatementKind::Block(statements)) =
                (&parameter, &catch_body.kind)
            {
                let mut reserved = HashSet::new();
                reserved.insert(parameter.clone());
                validate_declarations(statements, &reserved)?;
            }
            let catch_span = catch_start.join(catch_body.span);
            Some(CatchClause {
                parameter,
                body: catch_body,
                span: catch_span,
            })
        } else {
            None
        };
        let finally = if self.take(&TokenKind::Finally).is_some() {
            if !self.at(&TokenKind::LeftBrace) {
                return Err(self.error_here("finally body must be a block"));
            }
            Some(Box::new(self.parse_block()?))
        } else {
            None
        };
        if catch.is_none() && finally.is_none() {
            return Err(SyntaxIssue::new(
                "try requires catch or finally",
                start.join(body.span),
            ));
        }
        let end = finally.as_ref().map_or_else(
            || catch.as_ref().map_or(body.span, |clause| clause.span),
            |stmt| stmt.span,
        );
        Ok(Statement {
            kind: StatementKind::Try {
                body,
                catch,
                finally,
            },
            span: start.join(end),
        })
    }

    fn parse_loop_control(&mut self, is_break: bool) -> Result<Statement, SyntaxIssue> {
        let keyword = self.advance().clone();
        if self.loop_depth == 0 {
            return Err(SyntaxIssue::new(
                if is_break {
                    "break is only valid inside a loop"
                } else {
                    "continue is only valid inside a loop"
                },
                keyword.span,
            ));
        }
        if !self.has_line_terminator_after(&keyword)
            && matches!(self.peek().kind, TokenKind::Identifier(_))
        {
            return Err(self.error_here("labelled loop control is not implemented"));
        }
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: if is_break {
                StatementKind::Break
            } else {
                StatementKind::Continue
            },
            span: keyword.span.join(end),
        })
    }

    fn parse_expression_statement(&mut self) -> Result<Statement, SyntaxIssue> {
        let expression = self.parse_assignment()?;
        let start = expression.span;
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: StatementKind::Expression(expression),
            span: start.join(end),
        })
    }

    fn parse_assignment(&mut self) -> Result<Expression, SyntaxIssue> {
        let left = self.parse_logical_or()?;
        if self.take(&TokenKind::Assign).is_none() {
            return Ok(left);
        }
        let target = match left.kind {
            ExpressionKind::Identifier(name) => AssignmentTarget::Identifier(name),
            ExpressionKind::Member { object, property } => {
                AssignmentTarget::Member { object, property }
            }
            _ => {
                return Err(SyntaxIssue::new("invalid assignment target", left.span));
            }
        };
        let value = Box::new(self.parse_assignment()?);
        let span = left.span.join(value.span);
        Ok(Expression {
            kind: ExpressionKind::Assignment { target, value },
            span,
        })
    }

    fn parse_logical_or(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_logical_and()?;
        while self.take(&TokenKind::LogicalOr).is_some() {
            let right = self.parse_logical_and()?;
            let span = expression.span.join(right.span);
            expression = Expression {
                kind: ExpressionKind::Logical {
                    operator: LogicalOperator::Or,
                    left: Box::new(expression),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(expression)
    }

    fn parse_logical_and(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_equality()?;
        while self.take(&TokenKind::LogicalAnd).is_some() {
            let right = self.parse_equality()?;
            let span = expression.span.join(right.span);
            expression = Expression {
                kind: ExpressionKind::Logical {
                    operator: LogicalOperator::And,
                    left: Box::new(expression),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(expression)
    }

    fn parse_equality(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_relational()?;
        loop {
            let operator = if self.take(&TokenKind::StrictEqual).is_some() {
                Some(BinaryOperator::StrictEqual)
            } else if self.take(&TokenKind::StrictNotEqual).is_some() {
                Some(BinaryOperator::StrictNotEqual)
            } else if self.take(&TokenKind::Equal).is_some() {
                Some(BinaryOperator::Equal)
            } else if self.take(&TokenKind::NotEqual).is_some() {
                Some(BinaryOperator::NotEqual)
            } else {
                None
            };
            let Some(operator) = operator else { break };
            let right = self.parse_relational()?;
            expression = binary(expression, operator, right);
        }
        Ok(expression)
    }

    fn parse_relational(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_additive()?;
        loop {
            let operator = if self.take(&TokenKind::LessThan).is_some() {
                Some(BinaryOperator::LessThan)
            } else if self.take(&TokenKind::LessThanOrEqual).is_some() {
                Some(BinaryOperator::LessThanOrEqual)
            } else if self.take(&TokenKind::GreaterThan).is_some() {
                Some(BinaryOperator::GreaterThan)
            } else if self.take(&TokenKind::GreaterThanOrEqual).is_some() {
                Some(BinaryOperator::GreaterThanOrEqual)
            } else {
                None
            };
            let Some(operator) = operator else { break };
            let right = self.parse_additive()?;
            expression = binary(expression, operator, right);
        }
        Ok(expression)
    }

    fn parse_additive(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_multiplicative()?;
        loop {
            let operator = if self.take(&TokenKind::Plus).is_some() {
                Some(BinaryOperator::Add)
            } else if self.take(&TokenKind::Minus).is_some() {
                Some(BinaryOperator::Subtract)
            } else {
                None
            };
            let Some(operator) = operator else { break };
            let right = self.parse_multiplicative()?;
            expression = binary(expression, operator, right);
        }
        Ok(expression)
    }

    fn parse_multiplicative(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_unary()?;
        loop {
            let operator = if self.take(&TokenKind::Star).is_some() {
                Some(BinaryOperator::Multiply)
            } else if self.take(&TokenKind::Slash).is_some() {
                Some(BinaryOperator::Divide)
            } else if self.take(&TokenKind::Percent).is_some() {
                Some(BinaryOperator::Remainder)
            } else {
                None
            };
            let Some(operator) = operator else { break };
            let right = self.parse_unary()?;
            expression = binary(expression, operator, right);
        }
        Ok(expression)
    }

    fn parse_unary(&mut self) -> Result<Expression, SyntaxIssue> {
        let operator = if self.take(&TokenKind::Plus).is_some() {
            Some(UnaryOperator::Plus)
        } else if self.take(&TokenKind::Minus).is_some() {
            Some(UnaryOperator::Minus)
        } else if self.take(&TokenKind::Bang).is_some() {
            Some(UnaryOperator::Not)
        } else {
            None
        };
        if let Some(operator) = operator {
            let start = self.previous().span;
            let operand = Box::new(self.parse_unary()?);
            let span = start.join(operand.span);
            return Ok(Expression {
                kind: ExpressionKind::Unary { operator, operand },
                span,
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expression, SyntaxIssue> {
        let mut expression = self.parse_primary()?;
        loop {
            if self.take(&TokenKind::Dot).is_some() {
                let token = self.advance().clone();
                let Some(name) = token_as_property_name(&token.kind) else {
                    return Err(SyntaxIssue::new(
                        "expected a property name after '.'",
                        token.span,
                    ));
                };
                let span = expression.span.join(token.span);
                expression = Expression {
                    kind: ExpressionKind::Member {
                        object: Box::new(expression),
                        property: MemberProperty::Named(name),
                    },
                    span,
                };
            } else if self.take(&TokenKind::LeftBracket).is_some() {
                let property = self.parse_assignment()?;
                let end = self
                    .expect(
                        &TokenKind::RightBracket,
                        "expected ']' after property expression",
                    )?
                    .span;
                let span = expression.span.join(end);
                expression = Expression {
                    kind: ExpressionKind::Member {
                        object: Box::new(expression),
                        property: MemberProperty::Computed(Box::new(property)),
                    },
                    span,
                };
            } else if self.take(&TokenKind::LeftParen).is_some() {
                let mut arguments = Vec::new();
                if !self.at(&TokenKind::RightParen) {
                    loop {
                        arguments.push(self.parse_assignment()?);
                        if self.take(&TokenKind::Comma).is_none() {
                            break;
                        }
                    }
                }
                let end = self
                    .expect(&TokenKind::RightParen, "expected ')' after arguments")?
                    .span;
                let span = expression.span.join(end);
                expression = Expression {
                    kind: ExpressionKind::Call {
                        callee: Box::new(expression),
                        arguments,
                    },
                    span,
                };
            } else {
                break;
            }
        }
        Ok(expression)
    }

    fn parse_primary(&mut self) -> Result<Expression, SyntaxIssue> {
        let token = self.advance().clone();
        let (kind, span) = match token.kind {
            TokenKind::Number(value) => {
                (ExpressionKind::Literal(Literal::Number(value)), token.span)
            }
            TokenKind::String(value) => {
                (ExpressionKind::Literal(Literal::String(value)), token.span)
            }
            TokenKind::True => (ExpressionKind::Literal(Literal::Boolean(true)), token.span),
            TokenKind::False => (ExpressionKind::Literal(Literal::Boolean(false)), token.span),
            TokenKind::Null => (ExpressionKind::Literal(Literal::Null), token.span),
            TokenKind::Identifier(name) => (ExpressionKind::Identifier(name), token.span),
            TokenKind::This => (ExpressionKind::This, token.span),
            TokenKind::Function => {
                let function = self.parse_function_after_keyword(token.span, false)?;
                let span = function.span;
                (ExpressionKind::Function(function), span)
            }
            TokenKind::LeftBrace => return self.parse_object_literal(token.span),
            TokenKind::LeftParen => {
                let mut expression = self.parse_assignment()?;
                let end = self
                    .expect(&TokenKind::RightParen, "expected ')' after expression")?
                    .span;
                expression.span = token.span.join(end);
                return Ok(expression);
            }
            _ => return Err(SyntaxIssue::new("expected an expression", token.span)),
        };
        Ok(Expression { kind, span })
    }

    fn parse_object_literal(&mut self, start: SourceSpan) -> Result<Expression, SyntaxIssue> {
        let mut properties = Vec::new();
        if !self.at(&TokenKind::RightBrace) {
            loop {
                let key_token = self.advance().clone();
                let (key, shorthand) = match key_token.kind {
                    TokenKind::Identifier(name) => (name, true),
                    TokenKind::String(name) => (name, false),
                    TokenKind::Number(number) => (number_to_property_key(number), false),
                    _ => {
                        let Some(name) = token_as_property_name(&key_token.kind) else {
                            return Err(SyntaxIssue::new(
                                "expected an object property name",
                                key_token.span,
                            ));
                        };
                        (name, false)
                    }
                };
                let value = if self.take(&TokenKind::Colon).is_some() {
                    self.parse_assignment()?
                } else if shorthand {
                    Expression {
                        kind: ExpressionKind::Identifier(key.clone()),
                        span: key_token.span,
                    }
                } else {
                    return Err(self.error_here("expected ':' after object property name"));
                };
                properties.push(ObjectProperty { key, value });
                if self.take(&TokenKind::Comma).is_none() {
                    break;
                }
                if self.at(&TokenKind::RightBrace) {
                    break;
                }
            }
        }
        let end = self
            .expect(&TokenKind::RightBrace, "expected '}' after object literal")?
            .span;
        Ok(Expression {
            kind: ExpressionKind::Object(properties),
            span: start.join(end),
        })
    }

    fn consume_semicolon(&mut self) -> Result<SourceSpan, SyntaxIssue> {
        if let Some(token) = self.take(&TokenKind::Semicolon) {
            return Ok(token.span);
        }
        let previous = self.previous().span;
        if self.at(&TokenKind::Eof)
            || self.at(&TokenKind::RightBrace)
            || self.peek().span.start.line > previous.end.line
        {
            Ok(previous)
        } else {
            Err(self.error_here("expected ';' or a line terminator"))
        }
    }

    fn has_line_terminator_after(&self, token: &Token) -> bool {
        self.peek().span.start.line > token.span.end.line
    }

    fn at(&self, expected: &TokenKind) -> bool {
        same_variant(&self.peek().kind, expected)
    }

    fn take(&mut self, expected: &TokenKind) -> Option<&Token> {
        if self.at(expected) {
            self.current += 1;
            Some(self.previous())
        } else {
            None
        }
    }

    fn expect(&mut self, expected: &TokenKind, message: &str) -> Result<&Token, SyntaxIssue> {
        if self.at(expected) {
            self.current += 1;
            Ok(self.previous())
        } else {
            Err(self.error_here(message))
        }
    }

    fn advance(&mut self) -> &Token {
        if !self.at(&TokenKind::Eof) {
            self.current += 1;
        }
        self.previous()
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.current]
    }

    fn previous(&self) -> &Token {
        &self.tokens[self.current.saturating_sub(1)]
    }

    fn error_here(&self, message: &str) -> SyntaxIssue {
        SyntaxIssue::new(message, self.peek().span)
    }
}

fn binary(left: Expression, operator: BinaryOperator, right: Expression) -> Expression {
    let span = left.span.join(right.span);
    Expression {
        kind: ExpressionKind::Binary {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        },
        span,
    }
}

fn validate_declarations(
    statements: &[Statement],
    reserved: &HashSet<String>,
) -> Result<(), SyntaxIssue> {
    let mut names = reserved.clone();
    for statement in statements {
        match &statement.kind {
            StatementKind::LexicalDeclaration { bindings, .. } => {
                for binding in bindings {
                    if !names.insert(binding.name.clone()) {
                        return Err(SyntaxIssue::new(
                            format!("duplicate lexical declaration '{}'", binding.name),
                            binding.span,
                        ));
                    }
                }
            }
            StatementKind::FunctionDeclaration(function) => {
                let Some(name) = function.name.as_ref() else {
                    return Err(SyntaxIssue::new(
                        "function declaration requires a name",
                        function.span,
                    ));
                };
                if !names.insert(name.clone()) {
                    return Err(SyntaxIssue::new(
                        format!("duplicate lexical declaration '{name}'"),
                        function.span,
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn same_variant(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn token_as_property_name(token: &TokenKind) -> Option<String> {
    let name = match token {
        TokenKind::Identifier(name) => return Some(name.clone()),
        TokenKind::Let => "let",
        TokenKind::Const => "const",
        TokenKind::Function => "function",
        TokenKind::Return => "return",
        TokenKind::If => "if",
        TokenKind::Else => "else",
        TokenKind::While => "while",
        TokenKind::Break => "break",
        TokenKind::Continue => "continue",
        TokenKind::True => "true",
        TokenKind::False => "false",
        TokenKind::Null => "null",
        TokenKind::This => "this",
        TokenKind::Throw => "throw",
        TokenKind::Try => "try",
        TokenKind::Catch => "catch",
        TokenKind::Finally => "finally",
        _ => return None,
    };
    Some(name.to_owned())
}

fn number_to_property_key(number: f64) -> String {
    if number == 0.0 {
        "0".to_owned()
    } else {
        number.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::ast::{BinaryOperator, ExpressionKind, StatementKind};

    #[test]
    fn honors_operator_precedence() {
        let program = parse("1 + 2 * 3 === 7 || false;").unwrap();
        let StatementKind::Expression(expression) = &program.statements[0].kind else {
            panic!("expected expression statement");
        };
        let ExpressionKind::Logical { left, .. } = &expression.kind else {
            panic!("expected logical expression");
        };
        let ExpressionKind::Binary { operator, .. } = &left.kind else {
            panic!("expected equality expression");
        };
        assert_eq!(*operator, BinaryOperator::StrictEqual);
    }

    #[test]
    fn return_observes_line_terminator() {
        let program = parse("function f() { return\n1; }").unwrap();
        let StatementKind::FunctionDeclaration(function) = &program.statements[0].kind else {
            panic!("expected function");
        };
        assert!(matches!(function.body[0].kind, StatementKind::Return(None)));
    }

    #[test]
    fn rejects_duplicate_lexical_declarations() {
        let error = parse("{ let x = 1; const x = 2; }").unwrap_err();
        assert!(error.message.contains("duplicate lexical declaration"));
    }

    #[test]
    fn rejects_throw_line_terminator() {
        let error = parse("throw\n1;").unwrap_err();
        assert!(error.message.contains("line terminator"));
    }

    #[test]
    fn parses_chained_calls_and_members() {
        parse("factory().value[\"method\"](1, 2);").unwrap();
    }

    #[test]
    fn rejects_lexical_declaration_in_single_statement_position() {
        let error = parse("if (true) let value = 1;").unwrap_err();
        assert!(error.message.contains("require a statement-list block"));
    }

    #[test]
    fn accepts_empty_input() {
        let program = parse("").unwrap();
        assert!(program.statements.is_empty());
    }
}
