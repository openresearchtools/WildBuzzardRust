use crate::source::SourceSpan;

#[derive(Clone, Debug)]
pub(crate) struct Program {
    pub statements: Vec<Statement>,
}

#[derive(Clone, Debug)]
pub(crate) struct Statement {
    pub kind: StatementKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub(crate) enum StatementKind {
    Empty,
    Expression(Expression),
    LexicalDeclaration {
        kind: DeclarationKind,
        bindings: Vec<BindingDeclaration>,
    },
    Block(Vec<Statement>),
    If {
        test: Expression,
        consequent: Box<Statement>,
        alternate: Option<Box<Statement>>,
    },
    While {
        test: Expression,
        body: Box<Statement>,
    },
    Break,
    Continue,
    FunctionDeclaration(Function),
    Return(Option<Expression>),
    Throw(Expression),
    Try {
        body: Box<Statement>,
        catch: Option<CatchClause>,
        finally: Option<Box<Statement>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeclarationKind {
    Let,
    Const,
}

#[derive(Clone, Debug)]
pub(crate) struct BindingDeclaration {
    pub name: String,
    pub initializer: Option<Expression>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub(crate) struct CatchClause {
    pub parameter: Option<String>,
    pub body: Box<Statement>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub(crate) struct Function {
    pub name: Option<String>,
    pub parameters: Vec<String>,
    pub body: Vec<Statement>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub(crate) struct Expression {
    pub kind: ExpressionKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub(crate) enum ExpressionKind {
    Literal(Literal),
    Identifier(String),
    This,
    Unary {
        operator: UnaryOperator,
        operand: Box<Expression>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Logical {
        operator: LogicalOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Assignment {
        target: AssignmentTarget,
        value: Box<Expression>,
    },
    Function(Function),
    Object(Vec<ObjectProperty>),
    Member {
        object: Box<Expression>,
        property: MemberProperty,
    },
    Call {
        callee: Box<Expression>,
        arguments: Vec<Expression>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum Literal {
    Null,
    Boolean(bool),
    Number(f64),
    String(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnaryOperator {
    Plus,
    Minus,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    Equal,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogicalOperator {
    And,
    Or,
}

#[derive(Clone, Debug)]
pub(crate) enum AssignmentTarget {
    Identifier(String),
    Member {
        object: Box<Expression>,
        property: MemberProperty,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum MemberProperty {
    Named(String),
    Computed(Box<Expression>),
}

#[derive(Clone, Debug)]
pub(crate) struct ObjectProperty {
    pub key: String,
    pub value: Expression,
}
