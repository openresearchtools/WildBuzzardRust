use std::collections::{HashMap, HashSet};

use crate::ast::{
    AssignmentTarget, BinaryOperator, BindingDeclaration, CatchClause, DeclarationKind, Expression,
    ExpressionKind, ForInitializer, ForStatement, Function, Literal, LogicalOperator,
    MemberProperty, ObjectProperty, Program, Statement, StatementKind, UnaryOperator,
};
use crate::error::SyntaxIssue;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::source::SourceSpan;
use crate::string::JsString;

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
        validate_var_scope(&statements, &HashSet::new())?;
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
        if self.at(&TokenKind::Do) {
            return self.parse_do_while();
        }
        if self.at(&TokenKind::For) {
            return self.parse_for();
        }
        if self.at(&TokenKind::Var) {
            return self.parse_variable_declaration();
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
        let bindings = self.parse_binding_list(kind == DeclarationKind::Const)?;
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: StatementKind::LexicalDeclaration { kind, bindings },
            span: keyword.span.join(end),
        })
    }

    fn parse_variable_declaration(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::Var, "expected 'var'")?.span;
        let bindings = self.parse_binding_list(false)?;
        let end = self.consume_semicolon()?;
        Ok(Statement {
            kind: StatementKind::VariableDeclaration(bindings),
            span: start.join(end),
        })
    }

    fn parse_binding_list(
        &mut self,
        initializer_required: bool,
    ) -> Result<Vec<BindingDeclaration>, SyntaxIssue> {
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
            if initializer_required && initializer.is_none() {
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
        Ok(bindings)
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
        validate_var_scope(&body, &parameter_names)?;
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

    fn parse_do_while(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::Do, "expected 'do'")?.span;
        self.loop_depth += 1;
        let body_result = self.parse_statement();
        self.loop_depth -= 1;
        let body = Box::new(body_result?);
        self.expect(&TokenKind::While, "expected 'while' after do-while body")?;
        self.expect(&TokenKind::LeftParen, "expected '(' after 'while'")?;
        let test = self.parse_assignment()?;
        let end = self
            .expect(&TokenKind::RightParen, "expected ')' after condition")?
            .span;
        let end = self
            .take(&TokenKind::Semicolon)
            .map_or(end, |token| token.span);
        Ok(Statement {
            kind: StatementKind::DoWhile { body, test },
            span: start.join(end),
        })
    }

    fn parse_for(&mut self) -> Result<Statement, SyntaxIssue> {
        let start = self.expect(&TokenKind::For, "expected 'for'")?.span;
        self.expect(&TokenKind::LeftParen, "expected '(' after 'for'")?;

        let initializer = if self.take(&TokenKind::Semicolon).is_some() {
            None
        } else {
            let initializer = if self.take(&TokenKind::Var).is_some() {
                ForInitializer::Variable(self.parse_binding_list(false)?)
            } else if self.at(&TokenKind::Let) || self.at(&TokenKind::Const) {
                let keyword = self.advance().clone();
                let kind = if keyword.kind == TokenKind::Let {
                    DeclarationKind::Let
                } else {
                    DeclarationKind::Const
                };
                let bindings = self.parse_binding_list(kind == DeclarationKind::Const)?;
                ForInitializer::Lexical { kind, bindings }
            } else {
                ForInitializer::Expression(self.parse_assignment()?)
            };
            self.reject_for_in_or_of()?;
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after for-loop initializer",
            )?;
            Some(initializer)
        };

        let test = if self.take(&TokenKind::Semicolon).is_some() {
            None
        } else {
            let test = self.parse_assignment()?;
            self.expect(&TokenKind::Semicolon, "expected ';' after for-loop test")?;
            Some(test)
        };
        let update = if self.at(&TokenKind::RightParen) {
            None
        } else {
            Some(self.parse_assignment()?)
        };
        self.expect(&TokenKind::RightParen, "expected ')' after for-loop head")?;

        self.loop_depth += 1;
        let body_result = self.parse_statement();
        self.loop_depth -= 1;
        let body = Box::new(body_result?);
        let span = start.join(body.span);
        Ok(Statement {
            kind: StatementKind::For(Box::new(ForStatement {
                initializer,
                test,
                update,
                body,
            })),
            span,
        })
    }

    fn reject_for_in_or_of(&self) -> Result<(), SyntaxIssue> {
        if matches!(&self.peek().kind, TokenKind::Identifier(name) if name == "in" || name == "of")
        {
            return Err(self.error_here("for-in and for-of are not implemented"));
        }
        Ok(())
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
        if self.take(&TokenKind::Delete).is_some() {
            let start = self.previous().span;
            let operand = self.parse_unary()?;
            let span = start.join(operand.span);
            let ExpressionKind::Member { object, property } = operand.kind else {
                return Err(SyntaxIssue::new(
                    "only property deletion is implemented",
                    span,
                ));
            };
            return Ok(Expression {
                kind: ExpressionKind::Delete { object, property },
                span,
            });
        }
        let operator = if self.take(&TokenKind::Plus).is_some() {
            Some(UnaryOperator::Plus)
        } else if self.take(&TokenKind::Minus).is_some() {
            Some(UnaryOperator::Minus)
        } else if self.take(&TokenKind::Bang).is_some() {
            Some(UnaryOperator::Not)
        } else if self.take(&TokenKind::Typeof).is_some() {
            Some(UnaryOperator::Typeof)
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
        let expression = if self.take(&TokenKind::New).is_some() {
            let start = self.previous().span;
            self.parse_new_after_keyword(start)?
        } else {
            self.parse_primary()?
        };
        self.parse_postfix_suffix(expression, true)
    }

    fn parse_new_after_keyword(&mut self, start: SourceSpan) -> Result<Expression, SyntaxIssue> {
        let callee = if self.take(&TokenKind::New).is_some() {
            let nested_start = self.previous().span;
            self.parse_new_after_keyword(nested_start)?
        } else {
            self.parse_primary()?
        };
        let callee = self.parse_postfix_suffix(callee, false)?;
        let (arguments, end) = if self.take(&TokenKind::LeftParen).is_some() {
            let arguments = self.parse_arguments_after_left_paren()?;
            (arguments, self.previous().span)
        } else {
            (Vec::new(), callee.span)
        };
        Ok(Expression {
            kind: ExpressionKind::Construct {
                callee: Box::new(callee),
                arguments,
            },
            span: start.join(end),
        })
    }

    fn parse_postfix_suffix(
        &mut self,
        mut expression: Expression,
        allow_calls: bool,
    ) -> Result<Expression, SyntaxIssue> {
        loop {
            if self.take(&TokenKind::Dot).is_some() {
                let token = self.advance().clone();
                let Some(name) = token_as_property_name(&token.kind) else {
                    return Err(SyntaxIssue::new(
                        "expected a property name after '.'",
                        token.span,
                    ));
                };
                let name = property_name_from_utf8(name, token.span)?;
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
            } else if allow_calls && self.take(&TokenKind::LeftParen).is_some() {
                let arguments = self.parse_arguments_after_left_paren()?;
                let end = self.previous().span;
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

    fn parse_arguments_after_left_paren(&mut self) -> Result<Vec<Expression>, SyntaxIssue> {
        let mut arguments = Vec::new();
        if !self.at(&TokenKind::RightParen) {
            loop {
                arguments.push(self.parse_assignment()?);
                if self.take(&TokenKind::Comma).is_none() {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RightParen, "expected ')' after arguments")?;
        Ok(arguments)
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
            TokenKind::LeftBracket => return self.parse_array_literal(token.span),
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

    fn parse_array_literal(&mut self, start: SourceSpan) -> Result<Expression, SyntaxIssue> {
        let mut elements = Vec::new();
        while !self.at(&TokenKind::RightBracket) {
            if self.take(&TokenKind::Comma).is_some() {
                elements.push(None);
                continue;
            }
            elements.push(Some(self.parse_assignment()?));
            if self.take(&TokenKind::Comma).is_none() {
                break;
            }
            if self.at(&TokenKind::RightBracket) {
                break;
            }
        }
        let end = self
            .expect(&TokenKind::RightBracket, "expected ']' after array literal")?
            .span;
        Ok(Expression {
            kind: ExpressionKind::Array(elements),
            span: start.join(end),
        })
    }

    fn parse_object_literal(&mut self, start: SourceSpan) -> Result<Expression, SyntaxIssue> {
        let mut properties = Vec::new();
        if !self.at(&TokenKind::RightBrace) {
            loop {
                let key_token = self.advance().clone();
                let (key, shorthand) = match key_token.kind {
                    TokenKind::Identifier(name) => {
                        (property_name_from_utf8(&name, key_token.span)?, Some(name))
                    }
                    TokenKind::String(name) => (name, None),
                    TokenKind::Number(number) => (number_to_property_key(number), None),
                    _ => {
                        let Some(name) = token_as_property_name(&key_token.kind) else {
                            return Err(SyntaxIssue::new(
                                "expected an object property name",
                                key_token.span,
                            ));
                        };
                        (property_name_from_utf8(name, key_token.span)?, None)
                    }
                };
                let value = if self.take(&TokenKind::Colon).is_some() {
                    self.parse_assignment()?
                } else if let Some(shorthand) = shorthand {
                    Expression {
                        kind: ExpressionKind::Identifier(shorthand),
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

fn validate_var_scope(
    statements: &[Statement],
    reserved: &HashSet<String>,
) -> Result<(), SyntaxIssue> {
    let mut all_lexical_names = reserved.clone();
    let mut direct_lexical_names = HashSet::new();
    for statement in statements {
        if let StatementKind::LexicalDeclaration { bindings, .. } = &statement.kind {
            for binding in bindings {
                insert_lexical_name(&mut all_lexical_names, &binding.name, binding.span)?;
                direct_lexical_names.insert(binding.name.clone());
            }
        }
    }

    let mut var_names = HashMap::new();
    for statement in statements {
        if let StatementKind::FunctionDeclaration(function) = &statement.kind {
            let Some(name) = function.name.as_ref() else {
                return Err(SyntaxIssue::new(
                    "function declaration requires a name",
                    function.span,
                ));
            };
            insert_var_name(&mut var_names, name, function.span);
        } else {
            collect_statement_var_names(statement, &mut var_names)?;
        }
    }
    reject_var_lexical_conflicts(&var_names, &direct_lexical_names)
}

fn validate_lexical_scope(
    statements: &[Statement],
    reserved: &HashSet<String>,
) -> Result<HashMap<String, SourceSpan>, SyntaxIssue> {
    let mut lexical_names = reserved.clone();
    for statement in statements {
        match &statement.kind {
            StatementKind::LexicalDeclaration { bindings, .. } => {
                for binding in bindings {
                    insert_lexical_name(&mut lexical_names, &binding.name, binding.span)?;
                }
            }
            StatementKind::FunctionDeclaration(function) => {
                let Some(name) = function.name.as_ref() else {
                    return Err(SyntaxIssue::new(
                        "function declaration requires a name",
                        function.span,
                    ));
                };
                insert_lexical_name(&mut lexical_names, name, function.span)?;
            }
            _ => {}
        }
    }

    let mut var_names = HashMap::new();
    for statement in statements {
        if !matches!(statement.kind, StatementKind::FunctionDeclaration(_)) {
            collect_statement_var_names(statement, &mut var_names)?;
        }
    }
    reject_var_lexical_conflicts(&var_names, &lexical_names)?;
    Ok(var_names)
}

fn collect_statement_var_names(
    statement: &Statement,
    names: &mut HashMap<String, SourceSpan>,
) -> Result<(), SyntaxIssue> {
    match &statement.kind {
        StatementKind::VariableDeclaration(bindings) => {
            for binding in bindings {
                insert_var_name(names, &binding.name, binding.span);
            }
        }
        StatementKind::Block(statements) => {
            extend_var_names(names, validate_lexical_scope(statements, &HashSet::new())?);
        }
        StatementKind::If {
            consequent,
            alternate,
            ..
        } => {
            collect_statement_var_names(consequent, names)?;
            if let Some(alternate) = alternate {
                collect_statement_var_names(alternate, names)?;
            }
        }
        StatementKind::While { body, .. } | StatementKind::DoWhile { body, .. } => {
            collect_statement_var_names(body, names)?;
        }
        StatementKind::For(for_statement) => match &for_statement.initializer {
            Some(ForInitializer::Variable(bindings)) => {
                for binding in bindings {
                    insert_var_name(names, &binding.name, binding.span);
                }
                collect_statement_var_names(&for_statement.body, names)?;
            }
            Some(ForInitializer::Lexical { bindings, .. }) => {
                let mut lexical_names = HashSet::new();
                for binding in bindings {
                    insert_lexical_name(&mut lexical_names, &binding.name, binding.span)?;
                }
                let mut body_var_names = HashMap::new();
                collect_statement_var_names(&for_statement.body, &mut body_var_names)?;
                reject_var_lexical_conflicts(&body_var_names, &lexical_names)?;
                extend_var_names(names, body_var_names);
            }
            Some(ForInitializer::Expression(_)) | None => {
                collect_statement_var_names(&for_statement.body, names)?;
            }
        },
        StatementKind::Try {
            body,
            catch,
            finally,
        } => {
            collect_statement_var_names(body, names)?;
            if let Some(catch) = catch {
                let mut reserved = HashSet::new();
                if let Some(parameter) = &catch.parameter {
                    reserved.insert(parameter.clone());
                }
                let StatementKind::Block(statements) = &catch.body.kind else {
                    return Err(SyntaxIssue::new("catch body must be a block", catch.span));
                };
                extend_var_names(names, validate_lexical_scope(statements, &reserved)?);
            }
            if let Some(finally) = finally {
                collect_statement_var_names(finally, names)?;
            }
        }
        StatementKind::Empty
        | StatementKind::Expression(_)
        | StatementKind::LexicalDeclaration { .. }
        | StatementKind::Break
        | StatementKind::Continue
        | StatementKind::FunctionDeclaration(_)
        | StatementKind::Return(_)
        | StatementKind::Throw(_) => {}
    }
    Ok(())
}

fn insert_lexical_name(
    names: &mut HashSet<String>,
    name: &str,
    span: SourceSpan,
) -> Result<(), SyntaxIssue> {
    if !names.insert(name.to_owned()) {
        return Err(SyntaxIssue::new(
            format!("duplicate lexical declaration '{name}'"),
            span,
        ));
    }
    Ok(())
}

fn insert_var_name(names: &mut HashMap<String, SourceSpan>, name: &str, span: SourceSpan) {
    names.entry(name.to_owned()).or_insert(span);
}

fn extend_var_names(target: &mut HashMap<String, SourceSpan>, source: HashMap<String, SourceSpan>) {
    for (name, span) in source {
        target.entry(name).or_insert(span);
    }
}

fn reject_var_lexical_conflicts(
    var_names: &HashMap<String, SourceSpan>,
    lexical_names: &HashSet<String>,
) -> Result<(), SyntaxIssue> {
    if let Some((name, span)) = var_names
        .iter()
        .filter(|(name, _)| lexical_names.contains(*name))
        .min_by_key(|(_, span)| span.start.byte_offset)
    {
        return Err(SyntaxIssue::new(
            format!("variable declaration '{name}' conflicts with a lexical declaration"),
            *span,
        ));
    }
    Ok(())
}

fn same_variant(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn token_as_property_name(token: &TokenKind) -> Option<&str> {
    let name = match token {
        TokenKind::Identifier(name) => return Some(name),
        TokenKind::Let => "let",
        TokenKind::Const => "const",
        TokenKind::Var => "var",
        TokenKind::Function => "function",
        TokenKind::New => "new",
        TokenKind::Delete => "delete",
        TokenKind::Typeof => "typeof",
        TokenKind::Return => "return",
        TokenKind::If => "if",
        TokenKind::Else => "else",
        TokenKind::While => "while",
        TokenKind::Do => "do",
        TokenKind::For => "for",
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
    Some(name)
}

fn number_to_property_key(number: f64) -> JsString {
    let value = if number == 0.0 {
        "0".to_owned()
    } else {
        number.to_string()
    };
    JsString::from_runtime_utf8(&value)
}

fn property_name_from_utf8(value: &str, span: SourceSpan) -> Result<JsString, SyntaxIssue> {
    JsString::from_utf8(value).map_err(|error| SyntaxIssue::new(error.to_string(), span))
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
        for source in [
            "function f() { return\n1; }",
            "function f() { return\u{2028}1; }",
            "function f() { return\u{2029}1; }",
        ] {
            let program = parse(source).unwrap();
            let StatementKind::FunctionDeclaration(function) = &program.statements[0].kind else {
                panic!("expected function");
            };
            assert!(matches!(function.body[0].kind, StatementKind::Return(None)));
        }
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
    fn parses_array_elisions_construction_and_property_deletion() {
        let program = parse("[, 1, ,]; new Constructor(2).value; delete target.key;").unwrap();
        let StatementKind::Expression(array) = &program.statements[0].kind else {
            panic!("expected array expression statement");
        };
        let ExpressionKind::Array(elements) = &array.kind else {
            panic!("expected array literal");
        };
        assert_eq!(elements.len(), 3);
        assert!(elements[0].is_none());
        assert!(elements[1].is_some());
        assert!(elements[2].is_none());

        let StatementKind::Expression(member) = &program.statements[1].kind else {
            panic!("expected constructed member expression");
        };
        let ExpressionKind::Member { object, .. } = &member.kind else {
            panic!("expected member access after construction");
        };
        assert!(matches!(&object.kind, ExpressionKind::Construct { .. }));

        let StatementKind::Expression(delete) = &program.statements[2].kind else {
            panic!("expected delete expression statement");
        };
        assert!(matches!(&delete.kind, ExpressionKind::Delete { .. }));
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
