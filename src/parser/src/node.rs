use crate::ast::AstIndex;
use std::fmt;

pub type ConstantIndex = u32;

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Empty,
    Id(ConstantIndex),
    Lookup((LookupNode, Option<AstIndex>)), // lookup node, next node
    BoolTrue,
    BoolFalse,
    Number0,
    Number1,
    Int(ConstantIndex),
    Float(ConstantIndex),
    Str(ConstantIndex),
    Num2(Vec<AstIndex>),
    Num4(Vec<AstIndex>),
    List(Vec<AstIndex>),
    Tuple(Vec<AstIndex>),
    TempTuple(Vec<AstIndex>),
    Range {
        start: AstIndex,
        end: AstIndex,
        inclusive: bool,
    },
    RangeFrom {
        start: AstIndex,
    },
    RangeTo {
        end: AstIndex,
        inclusive: bool,
    },
    RangeFull,
    Map(Vec<(ConstantIndex, Option<AstIndex>)>),
    MainBlock {
        body: Vec<AstIndex>,
        local_count: usize,
    },
    Block(Vec<AstIndex>),
    Function(Function),
    Call {
        function: AstIndex,
        args: Vec<AstIndex>,
    },
    Import {
        from: Vec<ConstantIndex>,
        items: Vec<Vec<ConstantIndex>>,
    },
    Assign {
        target: AssignTarget,
        op: AssignOp,
        expression: AstIndex,
    },
    MultiAssign {
        targets: Vec<AssignTarget>,
        expression: AstIndex,
    },
    BinaryOp {
        op: AstOp,
        lhs: AstIndex,
        rhs: AstIndex,
    },
    If(AstIf),
    Match {
        expression: Option<AstIndex>,
        arms: Vec<MatchArm>,
    },
    Wildcard,
    Ellipsis(Option<ConstantIndex>),
    For(AstFor),
    Loop {
        body: AstIndex,
    },
    While {
        condition: AstIndex,
        body: AstIndex,
    },
    Until {
        condition: AstIndex,
        body: AstIndex,
    },
    Break,
    Continue,
    Return,
    ReturnExpression(AstIndex),
    Negate(AstIndex),
    Try(AstTry),
    Yield(AstIndex),
    Debug {
        expression_string: ConstantIndex,
        expression: AstIndex,
    },
}

impl Default for Node {
    fn default() -> Self {
        Node::Empty
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Node::*;
        match self {
            Empty => write!(f, "Empty"),
            Id(_) => write!(f, "Id"),
            Lookup(_) => write!(f, "Lookup"),
            BoolTrue => write!(f, "BoolTrue"),
            BoolFalse => write!(f, "BoolFalse"),
            Float(_) => write!(f, "Float"),
            Int(_) => write!(f, "Int"),
            Number0 => write!(f, "Number0"),
            Number1 => write!(f, "Number1"),
            Str(_) => write!(f, "Str"),
            Num2(_) => write!(f, "Num2"),
            Num4(_) => write!(f, "Num4"),
            List(_) => write!(f, "List"),
            Tuple(_) => write!(f, "Tuple"),
            TempTuple(_) => write!(f, "TempTuple"),
            Range { .. } => write!(f, "Range"),
            RangeFrom { .. } => write!(f, "RangeFrom"),
            RangeTo { .. } => write!(f, "RangeTo"),
            RangeFull => write!(f, "RangeFull"),
            Map(_) => write!(f, "Map"),
            MainBlock { .. } => write!(f, "MainBlock"),
            Block(_) => write!(f, "Block"),
            Negate(_) => write!(f, "Negate"),
            Function(_) => write!(f, "Function"),
            Call { .. } => write!(f, "Call"),
            Import { .. } => write!(f, "Import"),
            Assign { .. } => write!(f, "Assign"),
            MultiAssign { .. } => write!(f, "MultiAssign"),
            BinaryOp { .. } => write!(f, "BinaryOp"),
            If(_) => write!(f, "If"),
            Match { .. } => write!(f, "Match"),
            Wildcard => write!(f, "Wildcard"),
            Ellipsis(_) => write!(f, "Ellipsis"),
            For(_) => write!(f, "For"),
            While { .. } => write!(f, "While"),
            Until { .. } => write!(f, "Until"),
            Loop { .. } => write!(f, "Loop"),
            Break => write!(f, "Break"),
            Continue => write!(f, "Continue"),
            Return => write!(f, "Return"),
            ReturnExpression(_) => write!(f, "ReturnExpression"),
            Try { .. } => write!(f, "Try"),
            Yield { .. } => write!(f, "Yield"),
            Debug { .. } => write!(f, "Debug"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    pub args: Vec<AstIndex>,
    pub local_count: usize,
    // Any ID or lookup root that's accessed in a function and which wasn't previously assigned
    // locally, is either a global or needs to be captured. The compiler takes care of determining
    // if an access is a capture or not at the moment the function is created.
    pub accessed_non_locals: Vec<ConstantIndex>,
    pub body: AstIndex,
    pub is_instance_function: bool,
    pub is_variadic: bool,
    pub is_generator: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AstFor {
    pub args: Vec<Option<ConstantIndex>>,
    pub range: AstIndex,
    pub body: AstIndex,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AstIf {
    pub condition: AstIndex,
    pub then_node: AstIndex,
    pub else_if_blocks: Vec<(AstIndex, AstIndex)>,
    pub else_node: Option<AstIndex>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AstOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AstTry {
    pub try_block: AstIndex,
    pub catch_arg: Option<ConstantIndex>,
    pub catch_block: AstIndex,
    pub finally_block: Option<AstIndex>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AssignOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    Global,
    Local,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LookupNode {
    Root(AstIndex),
    Id(ConstantIndex),
    Index(AstIndex),
    Call(Vec<AstIndex>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AssignTarget {
    pub target_index: AstIndex,
    pub scope: Scope,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub patterns: Vec<AstIndex>,
    pub condition: Option<AstIndex>,
    pub expression: AstIndex,
}
