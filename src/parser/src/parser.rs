use {
    crate::{error::*, *},
    koto_lexer::{Lexer, Span, Token},
    std::{
        cmp::Ordering,
        collections::{HashMap, HashSet},
        iter::FromIterator,
        str::FromStr,
    },
};

macro_rules! make_internal_error {
    ($error:ident, $parser:expr) => {{
        ParserError::new(InternalError::$error.into(), $parser.lexer.span())
    }};
}

macro_rules! internal_error {
    ($error:ident, $parser:expr) => {{
        let error = make_internal_error!($error, $parser);
        #[cfg(panic_on_parser_error)]
        {
            panic!(error);
        }
        Err(error)
    }};
}

macro_rules! syntax_error {
    ($error:ident, $parser:expr) => {{
        let error = ParserError::new(SyntaxError::$error.into(), $parser.lexer.span());
        #[cfg(panic_on_parser_error)]
        {
            panic!(error);
        }
        Err(error)
    }};
}

fn trim_str(s: &str, trim_from_start: usize, trim_from_end: usize) -> &str {
    let start = trim_from_start;
    let end = s.len() - trim_from_end;
    &s[start..end]
}

fn f64_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < std::f64::EPSILON
}

#[derive(Debug, Default)]
struct Frame {
    top_level: bool,
    // IDs that have been assigned within the current frame
    ids_assigned_in_scope: HashSet<ConstantIndex>,
    // IDs and lookup roots which have been accessed without being locally assigned previously
    accessed_non_locals: HashSet<ConstantIndex>,
    // Due to single-pass parsing, we don't know while parsing an ID if it's the lhs of an
    // assignment or not. We have to wait until the expression is complete to determine if the
    // reference to an ID was non-local or not.
    // To achieve this, while an expression is being parsed we can maintain a running count:
    // +1 for reading, -1 for assignment. At the end of the expression, a positive count indicates
    // a non-local access.
    //
    // e.g.
    //
    // a is a local, it's on lhs of expression, so its a local assignment
    // || a = 1
    // (access count == 0: +1 -1)
    //
    // a is first accessed as a non-local before being assigned locally
    // || a = a
    // (access count == 1: +1 -1 +1)
    //
    // a is assigned locally twice from a non-local
    // || a = a = a
    // (access count == 1: +1 -1 +1 -1 +1)
    expression_id_accesses: HashMap<ConstantIndex, usize>,
}

impl Frame {
    fn local_count(&self) -> usize {
        self.ids_assigned_in_scope
            .difference(&self.accessed_non_locals)
            .count()
    }

    // Non-locals accessed in a nested frame need to be declared as also accessed in this
    // frame. This ensures that captures from the outer frame will be available when
    // creating the nested inner scope.
    fn add_nested_accessed_non_locals(&mut self, nested_frame: &Frame) {
        self.accessed_non_locals.extend(
            nested_frame
                .accessed_non_locals
                .difference(&self.ids_assigned_in_scope)
                .cloned(),
        );
    }

    fn increment_expression_access_for_id(&mut self, id: ConstantIndex) {
        *self.expression_id_accesses.entry(id).or_insert(0) += 1;
    }

    fn decrement_expression_access_for_id(&mut self, id: ConstantIndex) -> Result<(), ()> {
        match self.expression_id_accesses.get_mut(&id) {
            Some(entry) => {
                *entry -= 1;
                Ok(())
            }
            None => Err(()),
        }
    }

    fn finish_expressions(&mut self) {
        for (id, access_count) in self.expression_id_accesses.iter() {
            if *access_count > 0 && !self.ids_assigned_in_scope.contains(id) {
                self.accessed_non_locals.insert(*id);
            }
        }
        self.expression_id_accesses.clear();
    }
}

pub struct Parser<'source> {
    ast: Ast,
    constants: ConstantPool,
    lexer: Lexer<'source>,
    frame_stack: Vec<Frame>,
    options: Options,
}

impl<'source> Parser<'source> {
    pub fn parse(
        source: &'source str,
        options: Options,
    ) -> Result<(Ast, ConstantPool), ParserError> {
        let capacity_guess = source.len() / 4;
        let mut parser = Parser {
            ast: Ast::with_capacity(capacity_guess),
            constants: ConstantPool::new(),
            lexer: Lexer::new(source),
            frame_stack: Vec::new(),
            options,
        };

        let main_block = parser.parse_main_block()?;
        parser.ast.set_entry_point(main_block);

        Ok((parser.ast, parser.constants))
    }

    fn frame(&self) -> Result<&Frame, ParserError> {
        match self.frame_stack.last() {
            Some(frame) => Ok(frame),
            None => Err(ParserError::new(
                InternalError::MissingScope.into(),
                Span::default(),
            )),
        }
    }

    fn frame_mut(&mut self) -> Result<&mut Frame, ParserError> {
        match self.frame_stack.last_mut() {
            Some(frame) => Ok(frame),
            None => Err(ParserError::new(
                InternalError::MissingScope.into(),
                Span::default(),
            )),
        }
    }

    fn parse_main_block(&mut self) -> Result<AstIndex, ParserError> {
        self.frame_stack.push(Frame {
            top_level: true,
            ..Frame::default()
        });

        let mut body = Vec::new();
        while self.consume_until_next_token().is_some() {
            if let Some(expression) = self.parse_line()? {
                body.push(expression);
            } else {
                return syntax_error!(ExpectedExpressionInMainBlock, self);
            }
        }

        let result = self.ast.push(
            Node::MainBlock {
                body,
                local_count: self.frame()?.local_count(),
            },
            Span::default(), // TODO is there something better to do here? first->last position?
        )?;

        self.frame_stack.pop();
        Ok(result)
    }

    fn parse_function(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::Function) {
            return internal_error!(FunctionParseFailure, self);
        }

        let current_indent = self.lexer.current_indent();

        self.consume_token();

        let span_start = self.lexer.span().start;

        // args
        let mut args = Vec::new();
        loop {
            self.consume_until_next_token();
            if let Some(constant_index) = self.parse_id(true) {
                args.push(constant_index);
            } else {
                break;
            }
        }

        if self.skip_whitespace_and_next() != Some(Token::Function) {
            return syntax_error!(ExpectedFunctionArgsEnd, self);
        }

        let is_instance_function = match args.as_slice() {
            [first, ..] => self.constants.get_string(*first as usize) == "self",
            _ => false,
        };

        // body
        let mut function_frame = Frame::default();
        function_frame.ids_assigned_in_scope.extend(args.clone());
        self.frame_stack.push(function_frame);

        let body = match self.skip_whitespace_and_peek() {
            Some(Token::NewLineIndented) if self.lexer.next_indent() > current_indent => {
                if let Some(block) = self.parse_indented_map_or_block(current_indent)? {
                    block
                } else {
                    return internal_error!(FunctionParseFailure, self);
                }
            }
            _ => {
                if let Some(body) = self.parse_line()? {
                    body
                } else {
                    return syntax_error!(ExpectedFunctionBody, self);
                }
            }
        };

        let function_frame = self
            .frame_stack
            .pop()
            .ok_or_else(|| make_internal_error!(MissingScope, self))?;

        self.frame_mut()?
            .add_nested_accessed_non_locals(&function_frame);

        let local_count = function_frame.local_count();

        let span_end = self.lexer.span().end;

        let result = self.ast.push(
            Node::Function(Function {
                args,
                local_count,
                accessed_non_locals: Vec::from_iter(function_frame.accessed_non_locals),
                body,
                is_instance_function,
            }),
            Span {
                start: span_start,
                end: span_end,
            },
        )?;

        Ok(Some(result))
    }

    fn parse_line(&mut self) -> Result<Option<AstIndex>, ParserError> {
        let result = if let Some(for_loop) = self.parse_for_loop(None, true)? {
            for_loop
        } else if let Some(while_loop) = self.parse_while_loop(None, true)? {
            while_loop
        } else if let Some(until_loop) = self.parse_until_loop(None, true)? {
            until_loop
        } else if let Some(export_id) = self.parse_export_id()? {
            export_id
        } else if let Some(debug_expression) = self.parse_debug_expression()? {
            debug_expression
        } else {
            match self.peek_token() {
                Some(Token::Error) => {
                    return syntax_error!(UnexpectedToken, self);
                }
                Some(Token::Break) => {
                    self.consume_token();
                    self.push_node(Node::Break)?
                }
                Some(Token::Continue) => {
                    self.consume_token();
                    self.push_node(Node::Continue)?
                }
                Some(Token::Return) => {
                    self.consume_token();
                    if let Some(expression) = self.parse_primary_expressions(true)? {
                        self.push_node(Node::ReturnExpression(expression))?
                    } else {
                        self.push_node(Node::Return)?
                    }
                }
                _ => {
                    if let Some(result) = self.parse_primary_expressions(false)? {
                        result
                    } else {
                        return Ok(None);
                    }
                }
            }
        };

        self.frame_mut()?.finish_expressions();

        Ok(Some(result))
    }

    fn parse_primary_expressions(
        &mut self,
        allow_initial_indentation: bool,
    ) -> Result<Option<AstIndex>, ParserError> {
        let current_indent = self.lexer.current_indent();

        let mut expected_indent = None;

        if allow_initial_indentation
            && self.skip_whitespace_and_peek() == Some(Token::NewLineIndented)
        {
            self.consume_until_next_token();

            let indent = self.lexer.current_indent();
            if indent <= current_indent {
                return Ok(None);
            }

            expected_indent = Some(indent);

            if let Some(map_block) = self.parse_map_block(current_indent, expected_indent)? {
                return Ok(Some(map_block));
            }
        }

        if let Some(first) = self.parse_primary_expression()? {
            let mut expressions = vec![first];
            while let Some(Token::Separator) = self.skip_whitespace_and_peek() {
                self.consume_token();

                if self.skip_whitespace_and_peek() == Some(Token::NewLineIndented) {
                    self.consume_until_next_token();

                    let next_indent = self.lexer.next_indent();

                    if let Some(expected_indent) = expected_indent {
                        match next_indent.cmp(&expected_indent) {
                            Ordering::Less => break,
                            Ordering::Equal => {}
                            Ordering::Greater => return syntax_error!(UnexpectedIndentation, self),
                        }
                    } else if next_indent <= current_indent {
                        break;
                    } else {
                        expected_indent = Some(next_indent);
                    }
                }

                if let Some(next_expression) =
                    self.parse_primary_expression_with_lhs(Some(&expressions))?
                {
                    match self.ast.node(next_expression).node {
                        Node::Assign { .. }
                        | Node::MultiAssign { .. }
                        | Node::For(_)
                        | Node::While { .. }
                        | Node::Until { .. } => {
                            // These nodes will have consumed the expressions parsed expressions,
                            // so there's no further work to do.
                            // e.g.
                            //   x, y for x, y in a, b
                            //   a, b = c, d
                            //   a, b, c = x
                            return Ok(Some(next_expression));
                        }
                        _ => {}
                    }

                    expressions.push(next_expression);
                }
            }
            if expressions.len() == 1 {
                Ok(Some(first))
            } else {
                Ok(Some(self.push_node(Node::Expressions(expressions))?))
            }
        } else {
            Ok(None)
        }
    }

    fn parse_primary_expression_with_lhs(
        &mut self,
        lhs: Option<&[AstIndex]>,
    ) -> Result<Option<AstIndex>, ParserError> {
        self.parse_expression_start(lhs, 0)
    }

    fn parse_primary_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        self.parse_expression_start(None, 0)
    }

    fn parse_non_primary_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        self.parse_expression_start(None, 1)
    }

    fn parse_expression_start(
        &mut self,
        lhs: Option<&[AstIndex]>,
        min_precedence: u8,
    ) -> Result<Option<AstIndex>, ParserError> {
        let primary_expression = min_precedence == 0;

        let start_line = self.lexer.line_number();

        let expression_start = {
            // ID expressions are broken out to allow function calls in first position
            let expression = if let Some(expression) = self.parse_negatable_expression()? {
                Some(expression)
            } else if let Some(expression) = self.parse_id_expression(primary_expression)? {
                Some(expression)
            } else {
                self.parse_term(primary_expression)?
            };

            match self.peek_token() {
                Some(Token::Range) | Some(Token::RangeInclusive) => {
                    return self.parse_range(expression)
                }
                _ => match expression {
                    Some(expression) => expression,
                    None => return Ok(None),
                },
            }
        };

        let continue_expression = start_line == self.lexer.line_number();

        if continue_expression {
            if let Some(lhs) = lhs {
                let mut lhs_with_expression_start = lhs.to_vec();
                lhs_with_expression_start.push(expression_start);
                self.parse_expression_continued(&lhs_with_expression_start, min_precedence)
            } else {
                self.parse_expression_continued(&[expression_start], min_precedence)
            }
        } else {
            Ok(Some(expression_start))
        }
    }

    fn parse_expression_continued(
        &mut self,
        lhs: &[AstIndex],
        min_precedence: u8,
    ) -> Result<Option<AstIndex>, ParserError> {
        let primary_expression = min_precedence == 0;

        use Token::*;

        let last_lhs = match lhs {
            [last] => *last,
            [.., last] => *last,
            _ => return internal_error!(MissingContinuedExpressionLhs, self),
        };

        if let Some(next) = self.skip_whitespace_and_peek() {
            match next {
                NewLine | NewLineIndented => {
                    if let Some(maybe_operator) = self.peek_until_next_token() {
                        if operator_precedence(maybe_operator).is_some() {
                            self.consume_until_next_token();
                            return self.parse_expression_continued(lhs, min_precedence);
                        }
                    }
                }
                For => return self.parse_for_loop(Some(lhs), primary_expression),
                While => return self.parse_while_loop(Some(lhs), primary_expression),
                Until => return self.parse_until_loop(Some(lhs), primary_expression),
                Assign => return self.parse_assign_expression(lhs, AssignOp::Equal),
                AssignAdd => return self.parse_assign_expression(lhs, AssignOp::Add),
                AssignSubtract => return self.parse_assign_expression(lhs, AssignOp::Subtract),
                AssignMultiply => return self.parse_assign_expression(lhs, AssignOp::Multiply),
                AssignDivide => return self.parse_assign_expression(lhs, AssignOp::Divide),
                AssignModulo => return self.parse_assign_expression(lhs, AssignOp::Modulo),
                _ => {
                    if let Some((left_priority, right_priority)) = operator_precedence(next) {
                        if let Some(token_after_op) = self.peek_token_n(1) {
                            if token_is_whitespace(token_after_op)
                                && left_priority >= min_precedence
                            {
                                let op = self.consume_token().unwrap();

                                let current_indent = self.lexer.current_indent();

                                let rhs = if let Some(map_block) =
                                    self.parse_map_block(current_indent, None)?
                                {
                                    map_block
                                } else if let Some(rhs_expression) =
                                    self.parse_expression_start(None, right_priority)?
                                {
                                    rhs_expression
                                } else {
                                    return syntax_error!(ExpectedRhsExpression, self);
                                };

                                let op_node = self.push_ast_op(op, last_lhs, rhs)?;
                                return self.parse_expression_continued(&[op_node], min_precedence);
                            }
                        }
                    }
                }
            }
        }

        Ok(Some(last_lhs))
    }

    fn parse_assign_expression(
        &mut self,
        lhs: &[AstIndex],
        assign_op: AssignOp,
    ) -> Result<Option<AstIndex>, ParserError> {
        self.consume_token();

        let mut targets = Vec::new();

        let scope = if self.options.export_all_top_level && self.frame()?.top_level {
            Scope::Global
        } else {
            Scope::Local
        };

        for lhs_expression in lhs.iter() {
            match self.ast.node(*lhs_expression).node.clone() {
                Node::Id(id_index) => {
                    if matches!(assign_op, AssignOp::Equal) {
                        self.frame_mut()?
                            .decrement_expression_access_for_id(id_index)
                            .map_err(|_| make_internal_error!(UnexpectedIdInExpression, self))?;

                        if matches!(scope, Scope::Local) {
                            self.frame_mut()?.ids_assigned_in_scope.insert(id_index);
                        }
                    }
                }
                Node::Lookup(_) => {}
                _ => return syntax_error!(ExpectedAssignmentTarget, self),
            }

            targets.push(AssignTarget {
                target_index: *lhs_expression,
                scope,
            });
        }

        if targets.is_empty() {
            return internal_error!(MissingAssignmentTarget, self);
        }

        if let Some(rhs) = self.parse_primary_expressions(true)? {
            let node = if targets.len() == 1 {
                Node::Assign {
                    target: *targets.first().unwrap(),
                    op: assign_op,
                    expression: rhs,
                }
            } else {
                Node::MultiAssign {
                    targets,
                    expressions: rhs,
                }
            };
            Ok(Some(self.push_node(node)?))
        } else {
            syntax_error!(ExpectedRhsExpression, self)
        }
    }

    fn parse_id(&mut self, allow_placeholders: bool) -> Option<ConstantIndex> {
        match self.skip_whitespace_and_peek() {
            Some(Token::Id) => {
                self.consume_token();
                Some(self.constants.add_string(self.lexer.slice()) as u32)
            }
            Some(Token::Placeholder) if allow_placeholders => {
                self.consume_token();
                Some(self.constants.add_string(self.lexer.slice()) as u32)
            }
            _ => None,
        }
    }

    fn parse_id_expression(
        &mut self,
        primary_expression: bool,
    ) -> Result<Option<AstIndex>, ParserError> {
        if let Some(constant_index) = self.parse_id(primary_expression) {
            self.frame_mut()?
                .increment_expression_access_for_id(constant_index);

            let result = match self.peek_token() {
                Some(Token::Whitespace) if primary_expression => {
                    let start_span = self.lexer.span();
                    self.consume_token();

                    let id_index = self.push_node(Node::Id(constant_index))?;

                    if let Some(expression) = self.parse_non_primary_expression()? {
                        let mut args = vec![expression];

                        let current_line = self.lexer.line_number();
                        while let Some(expression) = self.parse_non_primary_expression()? {
                            args.push(expression);

                            if self.lexer.line_number() != current_line {
                                break;
                            }
                        }

                        self.push_node_with_start_span(
                            Node::Call {
                                function: id_index,
                                args,
                            },
                            start_span,
                        )?
                    } else {
                        id_index
                    }
                }
                Some(Token::ParenOpen) | Some(Token::ListStart) | Some(Token::Dot) => {
                    self.parse_lookup(constant_index, primary_expression)?
                }
                _ => self.push_node(Node::Id(constant_index))?,
            };

            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    fn parse_lookup(
        &mut self,
        id: ConstantIndex,
        primary_expression: bool,
    ) -> Result<AstIndex, ParserError> {
        let mut lookup = Vec::new();

        lookup.push(LookupNode::Id(id));

        loop {
            match self.peek_token() {
                Some(Token::ParenOpen) => {
                    let args = self.parse_parenthesized_args()?;
                    lookup.push(LookupNode::Call(args));
                }
                Some(Token::ListStart) => {
                    self.consume_token();

                    let index_expression = if let Some(index_expression) =
                        self.parse_non_primary_expression()?
                    {
                        match self.peek_token() {
                            Some(Token::Range) => {
                                self.consume_token();

                                if let Some(end_expression) = self.parse_non_primary_expression()? {
                                    self.push_node(Node::Range {
                                        start: index_expression,
                                        end: end_expression,
                                        inclusive: false,
                                    })?
                                } else {
                                    self.push_node(Node::RangeFrom {
                                        start: index_expression,
                                    })?
                                }
                            }
                            Some(Token::RangeInclusive) => {
                                self.consume_token();

                                if let Some(end_expression) = self.parse_non_primary_expression()? {
                                    self.push_node(Node::Range {
                                        start: index_expression,
                                        end: end_expression,
                                        inclusive: true,
                                    })?
                                } else {
                                    self.push_node(Node::RangeFrom {
                                        start: index_expression,
                                    })?
                                }
                            }
                            _ => index_expression,
                        }
                    } else {
                        match self.skip_whitespace_and_peek() {
                            Some(Token::Range) => {
                                self.consume_token();

                                if let Some(end_expression) = self.parse_non_primary_expression()? {
                                    self.push_node(Node::RangeTo {
                                        end: end_expression,
                                        inclusive: false,
                                    })?
                                } else {
                                    self.push_node(Node::RangeFull)?
                                }
                            }
                            Some(Token::RangeInclusive) => {
                                self.consume_token();

                                if let Some(end_expression) = self.parse_non_primary_expression()? {
                                    self.push_node(Node::RangeTo {
                                        end: end_expression,
                                        inclusive: true,
                                    })?
                                } else {
                                    self.push_node(Node::RangeFull)?
                                }
                            }
                            _ => return syntax_error!(ExpectedIndexExpression, self),
                        }
                    };

                    if let Some(Token::ListEnd) = self.skip_whitespace_and_peek() {
                        self.consume_token();
                        lookup.push(LookupNode::Index(index_expression));
                    } else {
                        return syntax_error!(ExpectedIndexEnd, self);
                    }
                }
                Some(Token::Dot) => {
                    self.consume_token();

                    if let Some(id_index) = self.parse_id(false) {
                        lookup.push(LookupNode::Id(id_index));
                    } else {
                        return syntax_error!(ExpectedMapKey, self);
                    }
                }
                Some(Token::Whitespace) if primary_expression => {
                    self.consume_token();

                    if let Some(expression) = self.parse_non_primary_expression()? {
                        let mut args = vec![expression];

                        let current_line = self.lexer.line_number();
                        while let Some(expression) = self.parse_non_primary_expression()? {
                            args.push(expression);

                            if self.lexer.line_number() != current_line {
                                break;
                            }
                        }

                        lookup.push(LookupNode::Call(args));
                    }

                    break;
                }
                _ => break,
            }
        }

        Ok(self.push_node(Node::Lookup(lookup))?)
    }

    fn parse_parenthesized_args(&mut self) -> Result<Vec<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::ParenOpen) {
            return internal_error!(ArgumentsParseFailure, self);
        }

        self.consume_token();

        let mut args = Vec::new();

        loop {
            self.consume_until_next_token();

            if let Some(expression) = self.parse_non_primary_expression()? {
                args.push(expression);
            } else {
                break;
            }
        }

        self.consume_until_next_token();
        if self.consume_token() == Some(Token::ParenClose) {
            Ok(args)
        } else {
            syntax_error!(ExpectedArgsEnd, self)
        }
    }

    fn parse_negatable_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::Subtract) {
            return Ok(None);
        }

        if self.peek_token_n(1) == Some(Token::Whitespace) {
            return Ok(None);
        }

        self.consume_token();

        let expression = match self.peek_token() {
            Some(Token::Id) => self.parse_id_expression(false)?,
            Some(Token::ParenOpen) => self.parse_nested_expression()?,
            _ => None,
        };

        match expression {
            Some(expression) => Ok(Some(self.push_node(Node::Negate(expression))?)),
            None => syntax_error!(ExpectedNegatableExpression, self),
        }
    }

    fn parse_range(&mut self, lhs: Option<AstIndex>) -> Result<Option<AstIndex>, ParserError> {
        use Node::{Range, RangeFrom, RangeFull, RangeTo};

        let inclusive = match self.peek_token() {
            Some(Token::Range) => false,
            Some(Token::RangeInclusive) => true,
            _ => return Ok(None),
        };

        self.consume_token();

        let rhs = self.parse_term(false)?;

        let node = match (lhs, rhs) {
            (Some(start), Some(end)) => Range {
                start,
                end,
                inclusive,
            },
            (Some(start), None) => RangeFrom { start },
            (None, Some(end)) => RangeTo { end, inclusive },
            (None, None) => RangeFull,
        };

        Ok(Some(self.push_node(node)?))
    }

    fn parse_export_id(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() == Some(Token::Export) {
            self.consume_token();

            if let Some(constant_index) = self.parse_id(false) {
                let export_id = self.push_node(Node::Id(constant_index))?;

                match self.skip_whitespace_and_peek() {
                    Some(Token::Assign) => {
                        self.consume_token();

                        if let Some(rhs) = self.parse_primary_expressions(true)? {
                            let node = Node::Assign {
                                target: AssignTarget {
                                    target_index: export_id,
                                    scope: Scope::Global,
                                },
                                op: AssignOp::Equal,
                                expression: rhs,
                            };

                            Ok(Some(self.push_node(node)?))
                        } else {
                            return syntax_error!(ExpectedRhsExpression, self);
                        }
                    }
                    Some(Token::NewLine) | Some(Token::NewLineIndented) => Ok(Some(export_id)),
                    _ => syntax_error!(UnexpectedTokenAfterExportId, self),
                }
            } else {
                syntax_error!(ExpectedExportExpression, self)
            }
        } else {
            Ok(None)
        }
    }

    fn parse_debug_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.peek_token() != Some(Token::Debug) {
            return Ok(None);
        }

        self.consume_token();

        let start_position = self.lexer.span().start;

        self.skip_whitespace_and_peek();

        let expression_source_start = self.lexer.source_position();
        let expression = if let Some(expression) = self.parse_primary_expressions(true)? {
            expression
        } else {
            return syntax_error!(ExpectedExpression, self);
        };

        let expression_source_end = self.lexer.source_position();

        let expression_string = self
            .constants
            .add_string(&self.lexer.source()[expression_source_start..expression_source_end])
            as u32;

        let result = self.ast.push(
            Node::Debug {
                expression_string,
                expression,
            },
            Span {
                start: start_position,
                end: self.lexer.span().end,
            },
        )?;

        Ok(Some(result))
    }

    fn parse_term(&mut self, primary_expression: bool) -> Result<Option<AstIndex>, ParserError> {
        use Node::*;

        let current_indent = self.lexer.current_indent();

        if let Some(token) = self.skip_whitespace_and_peek() {
            let result = match token {
                Token::True => {
                    self.consume_token();
                    self.push_node(BoolTrue)?
                }
                Token::False => {
                    self.consume_token();
                    self.push_node(BoolFalse)?
                }
                Token::ParenOpen => return self.parse_nested_expression(),
                Token::Number => {
                    self.consume_token();
                    match f64::from_str(self.lexer.slice()) {
                        Ok(n) => {
                            if f64_eq(n, 0.0) {
                                self.push_node(Number0)?
                            } else if f64_eq(n, 1.0) {
                                self.push_node(Number1)?
                            } else {
                                let constant_index = self.constants.add_f64(n) as u32;
                                self.push_node(Number(constant_index))?
                            }
                        }
                        Err(_) => {
                            return internal_error!(NumberParseFailure, self);
                        }
                    }
                }
                Token::Str => {
                    self.consume_token();
                    let s = trim_str(self.lexer.slice(), 1, 1);
                    let constant_index = self.constants.add_string(s) as u32;
                    self.push_node(Str(constant_index))?
                }
                Token::Id => return self.parse_id_expression(primary_expression),
                Token::ListStart => return self.parse_list(),
                Token::MapStart => {
                    self.consume_token();

                    let mut entries = Vec::new();

                    loop {
                        self.consume_until_next_token();

                        if let Some(key) = self.parse_id(false) {
                            if self.consume_token() != Some(Token::Colon) {
                                return syntax_error!(ExpectedMapSeparator, self);
                            }

                            self.consume_until_next_token();
                            if let Some(value) = self.parse_primary_expression()? {
                                entries.push((key, value));
                            } else {
                                return syntax_error!(ExpectedMapValue, self);
                            }

                            if self.skip_whitespace_and_peek() == Some(Token::Separator) {
                                self.consume_token();
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }

                    if self.skip_whitespace_and_next() != Some(Token::MapEnd) {
                        return syntax_error!(ExpectedMapEnd, self);
                    }

                    self.push_node(Map(entries))?
                }
                Token::Num2 => {
                    self.consume_token();

                    let args = if self.peek_token() == Some(Token::ParenOpen) {
                        self.parse_parenthesized_args()?
                    } else {
                        let mut args = Vec::new();
                        while let Some(arg) = self.parse_term(false)? {
                            args.push(arg);
                        }
                        args
                    };

                    if args.is_empty() {
                        return syntax_error!(ExpectedExpression, self);
                    } else if args.len() > 2 {
                        return syntax_error!(TooManyNum2Terms, self);
                    }

                    self.push_node(Num2(args))?
                }
                Token::Num4 => {
                    self.consume_token();

                    let args = if self.peek_token() == Some(Token::ParenOpen) {
                        self.parse_parenthesized_args()?
                    } else {
                        let mut args = Vec::new();
                        while let Some(arg) = self.parse_term(false)? {
                            args.push(arg);
                        }
                        args
                    };

                    if args.is_empty() {
                        return syntax_error!(ExpectedExpression, self);
                    } else if args.len() > 4 {
                        return syntax_error!(TooManyNum4Terms, self);
                    }

                    self.push_node(Num4(args))?
                }
                Token::If => return self.parse_if_expression(),
                Token::Function => return self.parse_function(),
                Token::Copy => {
                    self.consume_token();
                    if let Some(expression) = self.parse_primary_expression()? {
                        self.push_node(Node::CopyExpression(expression))?
                    } else {
                        return syntax_error!(ExpectedExpression, self);
                    }
                }
                Token::Not => {
                    self.consume_token();
                    if let Some(expression) = self.parse_primary_expression()? {
                        self.push_node(Node::Negate(expression))?
                    } else {
                        return syntax_error!(ExpectedExpression, self);
                    }
                }
                Token::Import => return self.parse_import_expression(),
                Token::NewLineIndented => return self.parse_map_block(current_indent, None),
                Token::Error => return syntax_error!(LexerError, self),
                _ => return Ok(None),
            };

            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    fn parse_list(&mut self) -> Result<Option<AstIndex>, ParserError> {
        self.consume_token();

        let mut entries = Vec::new();

        loop {
            if self.consume_until_next_token() == Some(Token::ListEnd) {
                break;
            }

            if let Some(range) = self.parse_range(None)? {
                entries.push(range);
            } else {
                if !entries.is_empty() {
                    let comprehension = if let Some(for_loop) =
                        self.parse_for_loop(Some(&entries), false)?
                    {
                        Some(for_loop)
                    } else if let Some(while_loop) = self.parse_while_loop(Some(&entries), false)? {
                        Some(while_loop)
                    } else if let Some(until_loop) = self.parse_until_loop(Some(&entries), false)? {
                        Some(until_loop)
                    } else {
                        None
                    };

                    if let Some(comprehension) = comprehension {
                        entries.clear();
                        entries.push(comprehension);
                        break;
                    }
                }

                if let Some(term) = self.parse_term(false)? {
                    if matches!(
                        self.peek_token(),
                        Some(Token::Range) | Some(Token::RangeInclusive)
                    ) {
                        if let Some(range) = self.parse_range(Some(term))? {
                            entries.push(range);
                        } else {
                            return internal_error!(RangeParseFailure, self);
                        }
                    } else {
                        entries.push(term);
                    }
                } else {
                    break;
                }
            }
        }

        self.consume_until_next_token();

        if self.consume_token() != Some(Token::ListEnd) {
            return syntax_error!(ExpectedListEnd, self);
        }

        Ok(Some(self.push_node(Node::List(entries))?))
    }

    fn parse_indented_map_or_block(
        &mut self,
        current_indent: usize,
    ) -> Result<Option<AstIndex>, ParserError> {
        self.consume_until_next_token();
        let expected_indent = self.lexer.next_indent();

        let result =
            if let Some(map_block) = self.parse_map_block(current_indent, Some(expected_indent))? {
                Some(map_block)
            } else if let Some(block) =
                self.parse_indented_block(current_indent, Some(expected_indent))?
            {
                Some(block)
            } else {
                None
            };

        Ok(result)
    }

    fn parse_map_block(
        &mut self,
        current_indent: usize,
        block_indent: Option<usize>,
    ) -> Result<Option<AstIndex>, ParserError> {
        let block_indent = match block_indent {
            Some(indent) => indent,
            None => {
                if self.skip_whitespace_and_peek() != Some(Token::NewLineIndented) {
                    return Ok(None);
                }

                let block_indent = self.lexer.next_indent();

                if block_indent <= current_indent {
                    return Ok(None);
                }

                self.consume_token();
                block_indent
            }
        };

        // Look ahead to check there's at least one map entry
        if self.consume_until_next_token() != Some(Token::Id) {
            return Ok(None);
        }
        if self.peek_token_n(1) != Some(Token::Colon) {
            return Ok(None);
        }

        let mut entries = Vec::new();

        while let Some(key) = self.parse_id(false) {
            if self.skip_whitespace_and_next() != Some(Token::Colon) {
                return syntax_error!(ExpectedMapSeparator, self);
            }

            if let Some(value) = self.parse_primary_expression()? {
                entries.push((key, value));
            } else {
                // If a value wasn't found on the same line as the key, scan ahead to the next
                // token (skipping newlines) and try again
                self.consume_until_next_token();
                if let Some(value) = self.parse_primary_expression()? {
                    entries.push((key, value));
                } else {
                    return syntax_error!(ExpectedMapValue, self);
                }
            }

            self.consume_until_next_token();

            let next_indent = self.lexer.next_indent();
            match next_indent.cmp(&block_indent) {
                Ordering::Less => break,
                Ordering::Equal => {}
                Ordering::Greater => return syntax_error!(UnexpectedIndentation, self),
            }
        }

        Ok(Some(self.ast.push(Node::Map(entries), Span::default())?))
    }

    fn parse_for_loop(
        &mut self,
        inline_body: Option<&[AstIndex]>,
        primary_expression: bool,
    ) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::For) {
            return Ok(None);
        }

        let current_indent = self.lexer.current_indent();

        self.consume_token();

        let mut args = Vec::new();
        while let Some(constant_index) = self.parse_id(true) {
            args.push(constant_index);
            self.frame_mut()?
                .ids_assigned_in_scope
                .insert(constant_index);
            if self.skip_whitespace_and_peek() == Some(Token::Separator) {
                self.consume_token();
            }
        }
        if args.is_empty() {
            return syntax_error!(ExpectedForArgs, self);
        }

        if self.skip_whitespace_and_next() != Some(Token::In) {
            return syntax_error!(ExpectedForInKeyword, self);
        }

        let mut ranges = Vec::new();
        while let Some(range) = self.parse_non_primary_expression()? {
            ranges.push(range);

            if self.skip_whitespace_and_peek() != Some(Token::Separator) {
                break;
            }

            self.consume_token();
        }
        if ranges.is_empty() {
            return syntax_error!(ExpectedForRanges, self);
        }

        let condition = if self.skip_whitespace_and_peek() == Some(Token::If) {
            self.consume_token();
            if let Some(condition) = self.parse_primary_expression()? {
                Some(condition)
            } else {
                return syntax_error!(ExpectedForCondition, self);
            }
        } else {
            None
        };

        let body = if let Some(expressions) = inline_body {
            match expressions {
                [] => return internal_error!(ForParseFailure, self),
                [expression] => *expression,
                [function, args @ ..] if !primary_expression => self.push_node(Node::Call {
                    function: *function,
                    args: args.to_vec(),
                })?,
                _ => self.push_node(Node::Expressions(expressions.to_vec()))?,
            }
        } else if let Some(body) = self.parse_indented_block(current_indent, None)? {
            body
        } else {
            return syntax_error!(ExpectedForBody, self);
        };

        let result = self.push_node(Node::For(AstFor {
            args,
            ranges,
            condition,
            body,
        }))?;

        Ok(Some(result))
    }

    fn parse_while_loop(
        &mut self,
        inline_body: Option<&[AstIndex]>,
        primary_expression: bool,
    ) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::While) {
            return Ok(None);
        }

        let current_indent = self.lexer.current_indent();
        self.consume_token();

        let condition = if let Some(condition) = self.parse_primary_expression()? {
            condition
        } else {
            return syntax_error!(ExpectedWhileCondition, self);
        };

        let body = if let Some(expressions) = inline_body {
            match expressions {
                [] => return internal_error!(ForParseFailure, self),
                [expression] => *expression,
                [function, args @ ..] if !primary_expression => self.push_node(Node::Call {
                    function: *function,
                    args: args.to_vec(),
                })?,
                _ => self.push_node(Node::Expressions(expressions.to_vec()))?,
            }
        } else if let Some(body) = self.parse_indented_block(current_indent, None)? {
            body
        } else {
            return syntax_error!(ExpectedWhileBody, self);
        };

        let result = self.push_node(Node::While { condition, body })?;
        Ok(Some(result))
    }

    fn parse_until_loop(
        &mut self,
        inline_body: Option<&[AstIndex]>,
        primary_expression: bool,
    ) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::Until) {
            return Ok(None);
        }

        let current_indent = self.lexer.current_indent();
        self.consume_token();

        let condition = if let Some(condition) = self.parse_primary_expression()? {
            condition
        } else {
            return syntax_error!(ExpectedUntilCondition, self);
        };

        let body = if let Some(expressions) = inline_body {
            match expressions {
                [] => return internal_error!(ForParseFailure, self),
                [expression] => *expression,
                [function, args @ ..] if !primary_expression => self.push_node(Node::Call {
                    function: *function,
                    args: args.to_vec(),
                })?,
                _ => self.push_node(Node::Expressions(expressions.to_vec()))?,
            }
        } else if let Some(body) = self.parse_indented_block(current_indent, None)? {
            body
        } else {
            return syntax_error!(ExpectedUntilBody, self);
        };

        let result = self.push_node(Node::Until { condition, body })?;
        Ok(Some(result))
    }

    fn parse_if_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.peek_token() != Some(Token::If) {
            return Ok(None);
        }

        let current_indent = self.lexer.current_indent();

        self.consume_token();
        let condition = match self.parse_primary_expression()? {
            Some(condition) => condition,
            None => return syntax_error!(ExpectedIfCondition, self),
        };

        let result = if self.skip_whitespace_and_peek() == Some(Token::Then) {
            self.consume_token();
            let then_node = match self.parse_primary_expressions(false)? {
                Some(then_node) => then_node,
                None => return syntax_error!(ExpectedThenExpression, self),
            };
            let else_node = if self.skip_whitespace_and_peek() == Some(Token::Else) {
                self.consume_token();
                match self.parse_primary_expressions(false)? {
                    Some(else_node) => Some(else_node),
                    None => return syntax_error!(ExpectedElseExpression, self),
                }
            } else {
                None
            };

            self.push_node(Node::If(AstIf {
                condition,
                then_node,
                else_if_blocks: vec![],
                else_node,
            }))?
        } else if let Some(then_node) = self.parse_indented_map_or_block(current_indent)? {
            let mut else_if_blocks = Vec::new();

            while self.lexer.current_indent() == current_indent {
                if let Some(Token::ElseIf) = self.skip_whitespace_and_peek() {
                    self.consume_token();
                    if let Some(else_if_condition) = self.parse_primary_expression()? {
                        if let Some(else_if_block) =
                            self.parse_indented_map_or_block(current_indent)?
                        {
                            else_if_blocks.push((else_if_condition, else_if_block));
                        } else {
                            return syntax_error!(ExpectedElseIfBlock, self);
                        }
                    } else {
                        return syntax_error!(ExpectedElseIfCondition, self);
                    }
                } else {
                    break;
                }
            }

            let else_node = if self.lexer.current_indent() == current_indent {
                if let Some(Token::Else) = self.skip_whitespace_and_peek() {
                    self.consume_token();
                    if let Some(else_block) = self.parse_indented_map_or_block(current_indent)? {
                        Some(else_block)
                    } else {
                        return syntax_error!(ExpectedElseBlock, self);
                    }
                } else {
                    None
                }
            } else {
                None
            };

            self.push_node(Node::If(AstIf {
                condition,
                then_node,
                else_if_blocks,
                else_node,
            }))?
        } else {
            return syntax_error!(ExpectedThenKeywordOrBlock, self);
        };

        Ok(Some(result))
    }

    fn parse_import_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.peek_token() != Some(Token::Import) {
            return Ok(None);
        }

        self.consume_token();

        let items = if self.skip_whitespace_and_peek() == Some(Token::ListStart) {
            self.consume_token();

            let first = match self.parse_id(false) {
                Some(id) => id,
                None => return syntax_error!(ExpectedIdInImportItemList, self),
            };

            let mut items = vec![first];

            while self.skip_whitespace_and_peek() != Some(Token::ListEnd) {
                let next_item = match self.parse_id(false) {
                    Some(id) => id,
                    None => return syntax_error!(ExpectedIdInImportItemList, self),
                };
                items.push(next_item);
            }

            if self.consume_token() != Some(Token::ListEnd) {
                return syntax_error!(ExpectedListEndInImportItemList, self);
            }

            if self.skip_whitespace_and_next() != Some(Token::From) {
                return syntax_error!(ExpectedFromAfterImportItemList, self);
            }

            for item in items.iter() {
                self.frame_mut()?.ids_assigned_in_scope.insert(*item);
            }

            items
        } else {
            vec![]
        };

        let module = match self.parse_id(false) {
            Some(first) => {
                let mut module = vec![first];
                while self.peek_token() == Some(Token::Dot) {
                    self.consume_token();
                    let child_module = match self.parse_id(false) {
                        Some(next) => next,
                        None => return syntax_error!(ExpectedImportModuleId, self),
                    };
                    module.push(child_module);
                }
                self.frame_mut()?
                    .ids_assigned_in_scope
                    .insert(*module.last().unwrap());
                module
            }
            None => return syntax_error!(ExpectedImportModuleId, self),
        };

        Ok(Some(self.push_node(Node::Import { module, items })?))
    }

    fn parse_indented_block(
        &mut self,
        current_indent: usize,
        block_indent: Option<usize>,
    ) -> Result<Option<AstIndex>, ParserError> {
        let block_indent = match block_indent {
            Some(indent) => indent,
            None => {
                if self.skip_whitespace_and_peek() != Some(Token::NewLineIndented) {
                    return Ok(None);
                }

                let block_indent = self.lexer.next_indent();

                if block_indent <= current_indent {
                    return Ok(None);
                }

                self.consume_token();
                block_indent
            }
        };

        if block_indent <= current_indent {
            return Ok(None);
        }

        let mut body = Vec::new();
        self.consume_until_next_token();

        while let Some(expression) = self.parse_line()? {
            body.push(expression);

            self.consume_until_next_token();

            let next_indent = self.lexer.current_indent();
            match next_indent.cmp(&block_indent) {
                Ordering::Less => break,
                Ordering::Equal => {}
                Ordering::Greater => return syntax_error!(UnexpectedIndentation, self),
            }
        }

        // If the body is a single expression then it doesn't need to be wrapped in a block
        if body.len() == 1 {
            Ok(Some(*body.first().unwrap()))
        } else {
            Ok(Some(self.ast.push(Node::Block(body), Span::default())?))
        }
    }

    fn parse_nested_expression(&mut self) -> Result<Option<AstIndex>, ParserError> {
        if self.skip_whitespace_and_peek() != Some(Token::ParenOpen) {
            return Ok(None);
        }

        self.consume_token();

        let expression = if let Some(expression) = self.parse_primary_expression()? {
            expression
        } else {
            self.push_node(Node::Empty)?
        };

        if let Some(Token::ParenClose) = self.peek_token() {
            self.consume_token();
            Ok(Some(expression))
        } else {
            syntax_error!(ExpectedCloseParen, self)
        }
    }

    fn push_ast_op(
        &mut self,
        op: Token,
        lhs: AstIndex,
        rhs: AstIndex,
    ) -> Result<AstIndex, ParserError> {
        use Token::*;
        let ast_op = match op {
            Add => AstOp::Add,
            Subtract => AstOp::Subtract,
            Multiply => AstOp::Multiply,
            Divide => AstOp::Divide,
            Modulo => AstOp::Modulo,

            Equal => AstOp::Equal,
            NotEqual => AstOp::NotEqual,

            Greater => AstOp::Greater,
            GreaterOrEqual => AstOp::GreaterOrEqual,
            Less => AstOp::Less,
            LessOrEqual => AstOp::LessOrEqual,

            And => AstOp::And,
            Or => AstOp::Or,

            _ => unreachable!(),
        };
        self.push_node(Node::BinaryOp {
            op: ast_op,
            lhs,
            rhs,
        })
    }

    fn peek_token(&mut self) -> Option<Token> {
        self.lexer.peek()
    }

    fn peek_token_n(&mut self, n: usize) -> Option<Token> {
        self.lexer.peek_n(n)
    }

    fn consume_token(&mut self) -> Option<Token> {
        self.lexer.next()
    }

    fn push_node(&mut self, node: Node) -> Result<AstIndex, ParserError> {
        self.ast.push(node, self.lexer.span())
    }

    fn push_node_with_start_span(
        &mut self,
        node: Node,
        start_span: Span,
    ) -> Result<AstIndex, ParserError> {
        self.ast.push(
            node,
            Span {
                start: start_span.start,
                end: self.lexer.span().end,
            },
        )
    }

    fn peek_until_next_token(&mut self) -> Option<Token> {
        let mut peek_count = 0;
        loop {
            let peeked = self.peek_token_n(peek_count);

            match peeked {
                Some(Token::Whitespace) => {}
                Some(Token::NewLine) => {}
                Some(Token::NewLineIndented) => {}
                Some(Token::NewLineSkipped) => {}
                Some(Token::CommentMulti) => {}
                Some(Token::CommentSingle) => {}
                Some(token) => return Some(token),
                None => return None,
            }

            peek_count += 1;
        }
    }

    fn consume_until_next_token(&mut self) -> Option<Token> {
        loop {
            let peeked = self.peek_token();

            match peeked {
                Some(Token::Whitespace) => {}
                Some(Token::NewLine) => {}
                Some(Token::NewLineIndented) => {}
                Some(Token::NewLineSkipped) => {}
                Some(Token::CommentMulti) => {}
                Some(Token::CommentSingle) => {}
                Some(token) => return Some(token),
                None => return None,
            }

            self.lexer.next();
            continue;
        }
    }

    fn skip_whitespace_and_peek(&mut self) -> Option<Token> {
        loop {
            let peeked = self.peek_token();

            match peeked {
                Some(Token::Whitespace) => {}
                Some(Token::NewLineSkipped) => {}
                Some(token) => return Some(token),
                None => return None,
            }

            self.lexer.next();
            continue;
        }
    }

    fn skip_whitespace_and_next(&mut self) -> Option<Token> {
        loop {
            let peeked = self.peek_token();

            match peeked {
                Some(Token::Whitespace) => {}
                Some(Token::NewLineSkipped) => {}
                Some(_) => return self.lexer.next(),
                None => return None,
            }

            self.lexer.next();
            continue;
        }
    }
}

fn operator_precedence(op: Token) -> Option<(u8, u8)> {
    use Token::*;
    let priority = match op {
        Or => (1, 2),
        And => (3, 4),
        // TODO, chained comparisons currently require right-associativity
        Equal | NotEqual => (6, 5),
        Greater | GreaterOrEqual | Less | LessOrEqual => (8, 7),
        Add | Subtract => (9, 10),
        Multiply | Divide | Modulo => (11, 12),
        _ => return None,
    };
    Some(priority)
}

fn token_is_whitespace(op: Token) -> bool {
    use Token::*;
    matches!(op, Whitespace | NewLine | NewLineIndented)
}

#[cfg(test)]
mod tests {
    use super::*;
    use {crate::constant_pool::Constant, Node::*};

    fn check_ast(source: &str, expected_ast: &[Node], expected_constants: Option<&[Constant]>) {
        check_ast_with_options(source, expected_ast, expected_constants, Options::default());
    }

    fn check_ast_with_options(
        source: &str,
        expected_ast: &[Node],
        expected_constants: Option<&[Constant]>,
        options: Options,
    ) {
        println!("{}", source);

        match Parser::parse(source, options) {
            Ok((ast, constants)) => {
                for (i, (ast_node, expected_node)) in
                    ast.nodes().iter().zip(expected_ast.iter()).enumerate()
                {
                    assert_eq!(ast_node.node, *expected_node, "Mismatch at position {}", i);
                }
                assert_eq!(
                    ast.nodes().len(),
                    expected_ast.len(),
                    "Node list length mismatch"
                );

                if let Some(expected_constants) = expected_constants {
                    for (constant, expected_constant) in
                        constants.iter().zip(expected_constants.iter())
                    {
                        assert_eq!(constant, *expected_constant);
                    }
                    assert_eq!(
                        constants.len(),
                        expected_constants.len(),
                        "Constant list length mismatch"
                    );
                }
            }
            Err(error) => panic!("{} - {}", error, error.span.start),
        }
    }

    mod values {
        use super::*;

        #[test]
        fn literals() {
            let source = "
true
false
1
1.5
\"hello\"
a
()";
            check_ast(
                source,
                &[
                    BoolTrue,
                    BoolFalse,
                    Number1,
                    Number(0),
                    Str(1),
                    Id(2),
                    Empty,
                    MainBlock {
                        body: vec![0, 1, 2, 3, 4, 5, 6],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Number(1.5),
                    Constant::Str("hello"),
                    Constant::Str("a"),
                ]),
            )
        }

        #[test]
        fn negatives() {
            let source = "\
-12.0
-a
-x[0]
-(1 + 1)";
            check_ast(
                source,
                &[
                    Number(0),
                    Id(1),
                    Negate(1),
                    Number0,
                    Lookup(vec![LookupNode::Id(2), LookupNode::Index(3)]),
                    Negate(4), // 5
                    Number1,
                    Number1,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 6,
                        rhs: 7,
                    },
                    Negate(8),
                    MainBlock {
                        body: vec![0, 2, 5, 9],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Number(-12.0),
                    Constant::Str("a"),
                    Constant::Str("x"),
                ]),
            )
        }

        #[test]
        fn list() {
            let source = "[0 n \"test\" n -1]";
            check_ast(
                source,
                &[
                    Number0,
                    Id(0),
                    Str(1),
                    Id(0),
                    Number(2),
                    List(vec![0, 1, 2, 3, 4]),
                    MainBlock {
                        body: vec![5],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("n"),
                    Constant::Str("test"),
                    Constant::Number(-1.0),
                ]),
            )
        }

        #[test]
        fn list_with_line_breaks() {
            let source = "\
x = [
  0
  1 0 1
  0
]";
            check_ast(
                source,
                &[
                    Id(0),
                    Number0,
                    Number1,
                    Number0,
                    Number1,
                    Number0, // 5
                    List(vec![1, 2, 3, 4, 5]),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 6,
                    },
                    MainBlock {
                        body: vec![7],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn map_inline() {
            let source = "\
{}
{foo: 42, bar: \"hello\"}";
            check_ast(
                source,
                &[
                    Map(vec![]),
                    Number(1),
                    Str(3),
                    Map(vec![(0, 1), (2, 2)]), // map entries are constant/ast index pairs
                    MainBlock {
                        body: vec![0, 3],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("foo"),
                    Constant::Number(42.0),
                    Constant::Str("bar"),
                    Constant::Str("hello"),
                ]),
            )
        }

        #[test]
        fn map_block() {
            let source = "\
x =
  foo: 42
  bar: \"hello\"
  baz:
    foo: 0
x";
            check_ast(
                source,
                &[
                    Id(0),     // x
                    Number(2), // 42
                    Str(4),    // "hello"
                    Number0,
                    Map(vec![(1, 3)]),                 // baz nested map
                    Map(vec![(1, 1), (3, 2), (5, 4)]), // 5 - map entries are constant/ast pairs
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 5,
                    },
                    Id(0),
                    MainBlock {
                        body: vec![6, 7],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("x"),
                    Constant::Str("foo"),
                    Constant::Number(42.0),
                    Constant::Str("bar"),
                    Constant::Str("hello"),
                    Constant::Str("baz"),
                ]),
            )
        }

        #[test]
        fn ranges() {
            let source = "\
0..1
0..=1
(0 + 1)..(1 + 1)
foo.bar..foo.baz";
            check_ast(
                source,
                &[
                    Number0,
                    Number1,
                    Range {
                        start: 0,
                        end: 1,
                        inclusive: false,
                    },
                    Number0,
                    Number1,
                    Range {
                        start: 3,
                        end: 4,
                        inclusive: true,
                    }, // 5
                    Number0,
                    Number1,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 6,
                        rhs: 7,
                    },
                    Number1,
                    Number1, // 10
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 9,
                        rhs: 10,
                    },
                    Range {
                        start: 8,
                        end: 11,
                        inclusive: false,
                    },
                    Lookup(vec![LookupNode::Id(0), LookupNode::Id(1)]),
                    Lookup(vec![LookupNode::Id(0), LookupNode::Id(2)]),
                    Range {
                        start: 13,
                        end: 14,
                        inclusive: false,
                    }, //15
                    MainBlock {
                        body: vec![2, 5, 12, 15],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("foo"),
                    Constant::Str("bar"),
                    Constant::Str("baz"),
                ]),
            )
        }

        #[test]
        fn lists_from_ranges() {
            let source = "\
[0..1]
[0..10 10..=0]";
            check_ast(
                source,
                &[
                    Number0,
                    Number1,
                    Range {
                        start: 0,
                        end: 1,
                        inclusive: false,
                    },
                    List(vec![2]),
                    Number0,
                    Number(0), // 5
                    Range {
                        start: 4,
                        end: 5,
                        inclusive: false,
                    },
                    Number(0),
                    Number0,
                    Range {
                        start: 7,
                        end: 8,
                        inclusive: true,
                    },
                    List(vec![6, 9]),
                    MainBlock {
                        body: vec![3, 10],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Number(10.0)]),
            )
        }

        #[test]
        fn num2() {
            let source = "\
num2 0
num2 1 x";
            check_ast(
                source,
                &[
                    Number0,
                    Num2(vec![0]),
                    Number1,
                    Id(0),
                    Num2(vec![2, 3]),
                    MainBlock {
                        body: vec![1, 4],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn num4() {
            let source = "\
num4 0
num4 1 x
num4 x 0 1 x";
            check_ast(
                source,
                &[
                    Number0,
                    Num4(vec![0]),
                    Number1,
                    Id(0),
                    Num4(vec![2, 3]),
                    Id(0), // 5
                    Number0,
                    Number1,
                    Id(0),
                    Num4(vec![5, 6, 7, 8]),
                    MainBlock {
                        body: vec![1, 4, 9],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn multiple_expressions() {
            let source = "0, 1, 0";
            check_ast(
                source,
                &[
                    Number0,
                    Number1,
                    Number0,
                    Expressions(vec![0, 1, 2]),
                    MainBlock {
                        body: vec![3],
                        local_count: 0,
                    },
                ],
                None,
            )
        }
    }

    mod assignment {
        use super::*;
        use crate::node::{AssignTarget, Scope};

        #[test]
        fn single() {
            let source = "a = 1";
            check_ast(
                source,
                &[
                    Id(0),
                    Number1,
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 1,
                    },
                    MainBlock {
                        body: vec![2],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("a")]),
            )
        }

        #[test]
        fn single_with_export_top_level_option() {
            let source = "a = 1";
            check_ast_with_options(
                source,
                &[
                    Id(0),
                    Number1,
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Global,
                        },
                        op: AssignOp::Equal,
                        expression: 1,
                    },
                    MainBlock {
                        body: vec![2],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("a")]),
                crate::Options {
                    export_all_top_level: true,
                },
            )
        }

        #[test]
        fn single_export() {
            let source = "export a = 1 + 1";
            check_ast(
                source,
                &[
                    Id(0),
                    Number1,
                    Number1,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 1,
                        rhs: 2,
                    },
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Global,
                        },
                        op: AssignOp::Equal,
                        expression: 3,
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("a")]),
            )
        }

        #[test]
        fn multi_2_to_1() {
            let source = "x = 1, 0";
            check_ast(
                source,
                &[
                    Id(0),
                    Number1,
                    Number0,
                    Expressions(vec![1, 2]),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 3,
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn multi_2_to_2() {
            let source = "x, y[0] = 1, 0";
            check_ast(
                source,
                &[
                    Id(0),
                    Number0,
                    Lookup(vec![LookupNode::Id(1), LookupNode::Index(1)]),
                    Number1,
                    Number0,
                    Expressions(vec![3, 4]),
                    MultiAssign {
                        targets: vec![
                            AssignTarget {
                                target_index: 0,
                                scope: Scope::Local,
                            },
                            AssignTarget {
                                target_index: 2,
                                scope: Scope::Local,
                            },
                        ],
                        expressions: 5,
                    },
                    MainBlock {
                        body: vec![6],
                        local_count: 1, // y is assumed to be non-local
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y")]),
            )
        }

        #[test]
        fn multi_2_to_2_with_linebreaks() {
            let source = "\
x, y =
  1,
  0,
x";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Number1,
                    Number0,
                    Expressions(vec![2, 3]),
                    MultiAssign {
                        targets: vec![
                            AssignTarget {
                                target_index: 0,
                                scope: Scope::Local,
                            },
                            AssignTarget {
                                target_index: 1,
                                scope: Scope::Local,
                            },
                        ],
                        expressions: 4,
                    }, // 5
                    Id(0),
                    MainBlock {
                        body: vec![5, 6],
                        local_count: 2,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y")]),
            )
        }

        #[test]
        fn multi_1_to_3_with_placeholder() {
            let source = "x, _, y = f()";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Id(2),
                    Lookup(vec![LookupNode::Id(3), LookupNode::Call(vec![])]),
                    MultiAssign {
                        targets: vec![
                            AssignTarget {
                                target_index: 0,
                                scope: Scope::Local,
                            },
                            AssignTarget {
                                target_index: 1,
                                scope: Scope::Local,
                            },
                            AssignTarget {
                                target_index: 2,
                                scope: Scope::Local,
                            },
                        ],
                        expressions: 3,
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 3,
                    },
                ],
                Some(&[
                    Constant::Str("x"),
                    Constant::Str("_"),
                    Constant::Str("y"),
                    Constant::Str("f"),
                ]),
            )
        }

        #[test]
        fn modify_assign() {
            let source = "\
x += 0
x -= 1
x *= 2
x /= 3
x %= 4";
            check_ast(
                source,
                &[
                    Id(0),
                    Number0,
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Add,
                        expression: 1,
                    },
                    Id(0),
                    Number1,
                    Assign {
                        target: AssignTarget {
                            target_index: 3,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Subtract,
                        expression: 4,
                    }, // 5
                    Id(0),
                    Number(1),
                    Assign {
                        target: AssignTarget {
                            target_index: 6,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Multiply,
                        expression: 7,
                    },
                    Id(0),
                    Number(2), // 10
                    Assign {
                        target: AssignTarget {
                            target_index: 9,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Divide,
                        expression: 10,
                    },
                    Id(0),
                    Number(3),
                    Assign {
                        target: AssignTarget {
                            target_index: 12,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Modulo,
                        expression: 13,
                    },
                    MainBlock {
                        body: vec![2, 5, 8, 11, 14],
                        local_count: 0,
                    }, // 15
                ],
                Some(&[
                    Constant::Str("x"),
                    Constant::Number(2.0),
                    Constant::Number(3.0),
                    Constant::Number(4.0),
                ]),
            )
        }
    }

    mod arithmetic {
        use super::*;

        #[test]
        fn addition_subtraction() {
            let source = "1 - 0 + 1";
            check_ast(
                source,
                &[
                    Number1,
                    Number0,
                    BinaryOp {
                        op: AstOp::Subtract,
                        lhs: 0,
                        rhs: 1,
                    },
                    Number1,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 2,
                        rhs: 3,
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 0,
                    },
                ],
                None,
            )
        }

        #[test]
        fn add_multiply() {
            let source = "1 + 0 * 1 + 0";
            check_ast(
                source,
                &[
                    Number1,
                    Number0,
                    Number1,
                    BinaryOp {
                        op: AstOp::Multiply,
                        lhs: 1,
                        rhs: 2,
                    },
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 0,
                        rhs: 3,
                    },
                    Number0, // 5
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 4,
                        rhs: 5,
                    },
                    MainBlock {
                        body: vec![6],
                        local_count: 0,
                    },
                ],
                None,
            )
        }

        #[test]
        fn with_parentheses() {
            let source = "(1 + 0) * (1 + 0)";
            check_ast(
                source,
                &[
                    Number1,
                    Number0,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 0,
                        rhs: 1,
                    },
                    Number1,
                    Number0,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 3,
                        rhs: 4,
                    },
                    BinaryOp {
                        op: AstOp::Multiply,
                        lhs: 2,
                        rhs: 5,
                    },
                    MainBlock {
                        body: vec![6],
                        local_count: 0,
                    },
                ],
                None,
            )
        }

        #[test]
        fn divide_modulo() {
            let source = "18 / 3 % 4";
            check_ast(
                source,
                &[
                    Number(0),
                    Number(1),
                    BinaryOp {
                        op: AstOp::Divide,
                        lhs: 0,
                        rhs: 1,
                    },
                    Number(2),
                    BinaryOp {
                        op: AstOp::Modulo,
                        lhs: 2,
                        rhs: 3,
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Number(18.0),
                    Constant::Number(3.0),
                    Constant::Number(4.0),
                ]),
            )
        }

        #[test]
        fn string_and_id() {
            let source = "\"hello\" + x";
            check_ast(
                source,
                &[
                    Str(0),
                    Id(1),
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 0,
                        rhs: 1,
                    },
                    MainBlock {
                        body: vec![2],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("hello"), Constant::Str("x")]),
            )
        }
    }

    mod logic {
        use super::*;

        #[test]
        fn and_or() {
            let source = "0 < 1 and 1 > 0 or true";
            check_ast(
                source,
                &[
                    Number0,
                    Number1,
                    BinaryOp {
                        op: AstOp::Less,
                        lhs: 0,
                        rhs: 1,
                    },
                    Number1,
                    Number0,
                    BinaryOp {
                        op: AstOp::Greater,
                        lhs: 3,
                        rhs: 4,
                    },
                    BinaryOp {
                        op: AstOp::And,
                        lhs: 2,
                        rhs: 5,
                    },
                    BoolTrue,
                    BinaryOp {
                        op: AstOp::Or,
                        lhs: 6,
                        rhs: 7,
                    },
                    MainBlock {
                        body: vec![8],
                        local_count: 0,
                    },
                ],
                None,
            )
        }

        #[test]
        fn chained_comparisons() {
            let source = "0 < 1 <= 1";
            check_ast(
                source,
                &[
                    Number0,
                    Number1,
                    Number1,
                    BinaryOp {
                        op: AstOp::LessOrEqual,
                        lhs: 1,
                        rhs: 2,
                    },
                    BinaryOp {
                        op: AstOp::Less,
                        lhs: 0,
                        rhs: 3,
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 0,
                    },
                ],
                None,
            )
        }
    }

    mod control_flow {
        use super::*;

        #[test]
        fn if_inline() {
            let source = "1 + if true then 0 else 1";
            check_ast(
                source,
                &[
                    Number1,
                    BoolTrue,
                    Number0,
                    Number1,
                    If(AstIf {
                        condition: 1,
                        then_node: 2,
                        else_if_blocks: vec![],
                        else_node: Some(3),
                    }),
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 0,
                        rhs: 4,
                    },
                    MainBlock {
                        body: vec![5],
                        local_count: 0,
                    },
                ],
                None,
            )
        }

        #[test]
        fn if_block() {
            let source = "\
a = if false
  0
else if true
  1
else if false
  0
else
  1
a";
            check_ast(
                source,
                &[
                    Id(0),
                    BoolFalse,
                    Number0,
                    BoolTrue,
                    Number1,
                    BoolFalse, // 5
                    Number0,
                    Number1,
                    If(AstIf {
                        condition: 1,
                        then_node: 2,
                        else_if_blocks: vec![(3, 4), (5, 6)],
                        else_node: Some(7),
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 8,
                    },
                    Id(0),
                    MainBlock {
                        body: vec![9, 10],
                        local_count: 1,
                    }, // 10
                ],
                None,
            )
        }

        #[test]
        fn if_inline_multi_expressions() {
            let source = "a, b = if true then 0, 1 else 1, 0";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    BoolTrue,
                    Number0,
                    Number1,
                    Expressions(vec![3, 4]), // 5
                    Number1,
                    Number0,
                    Expressions(vec![6, 7]),
                    If(AstIf {
                        condition: 2,
                        then_node: 5,
                        else_if_blocks: vec![],
                        else_node: Some(8),
                    }),
                    MultiAssign {
                        targets: vec![
                            AssignTarget {
                                target_index: 0,
                                scope: Scope::Local,
                            },
                            AssignTarget {
                                target_index: 1,
                                scope: Scope::Local,
                            },
                        ],
                        expressions: 9,
                    }, // 10
                    MainBlock {
                        body: vec![10],
                        local_count: 2,
                    },
                ],
                Some(&[Constant::Str("a"), Constant::Str("b")]),
            )
        }
    }

    mod loops {
        use super::*;

        #[test]
        fn for_inline() {
            let source = "x for x in 0..1";
            check_ast(
                source,
                &[
                    Id(0),
                    Number0,
                    Number1,
                    Range {
                        start: 1,
                        end: 2,
                        inclusive: false,
                    },
                    For(AstFor {
                        args: vec![0],
                        ranges: vec![3],
                        condition: None,
                        body: 0,
                    }),
                    MainBlock {
                        body: vec![4],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn for_inline_multi() {
            let source = "x, y for x, y in a, b";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Id(2),
                    Id(3),
                    Expressions(vec![0, 1]),
                    For(AstFor {
                        args: vec![0, 1],
                        ranges: vec![2, 3],
                        condition: None,
                        body: 4,
                    }), // 5
                    MainBlock {
                        body: vec![5],
                        local_count: 2,
                    },
                ],
                Some(&[
                    Constant::Str("x"),
                    Constant::Str("y"),
                    Constant::Str("a"),
                    Constant::Str("b"),
                ]),
            )
        }

        #[test]
        fn for_inline_conditional() {
            let source = "x for x in y if x == 0";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Id(0),
                    Number0,
                    BinaryOp {
                        op: AstOp::Equal,
                        lhs: 2,
                        rhs: 3,
                    },
                    For(AstFor {
                        args: vec![0],
                        ranges: vec![1],
                        condition: Some(4),
                        body: 0,
                    }), // 5
                    MainBlock {
                        body: vec![5],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y")]),
            )
        }

        #[test]
        fn for_block() {
            let source = "\
for x in y if x > 0
  f x";
            check_ast(
                source,
                &[
                    Id(1),
                    Id(0),
                    Number0,
                    BinaryOp {
                        op: AstOp::Greater,
                        lhs: 1,
                        rhs: 2,
                    },
                    Id(2),
                    Id(0), // 5
                    Call {
                        function: 4,
                        args: vec![5],
                    },
                    For(AstFor {
                        args: vec![0],   // constant 0
                        ranges: vec![0], // ast 0
                        condition: Some(3),
                        body: 6,
                    }),
                    MainBlock {
                        body: vec![7],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y"), Constant::Str("f")]),
            )
        }

        #[test]
        fn while_inline() {
            let source = "x while true";
            check_ast(
                source,
                &[
                    Id(0),
                    BoolTrue,
                    While {
                        condition: 1,
                        body: 0,
                    },
                    MainBlock {
                        body: vec![2],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn until_inline() {
            let source = "y until false";
            check_ast(
                source,
                &[
                    Id(0),
                    BoolFalse,
                    Until {
                        condition: 1,
                        body: 0,
                    },
                    MainBlock {
                        body: vec![2],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("y")]),
            )
        }

        #[test]
        fn while_block() {
            let source = "\
while x > y
  f x";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    BinaryOp {
                        op: AstOp::Greater,
                        lhs: 0,
                        rhs: 1,
                    },
                    Id(2),
                    Id(0),
                    Call {
                        function: 3,
                        args: vec![4],
                    }, // 5
                    While {
                        condition: 2,
                        body: 5,
                    },
                    MainBlock {
                        body: vec![6],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y"), Constant::Str("f")]),
            )
        }

        #[test]
        fn until_block() {
            let source = "\
until x < y
  f y";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    BinaryOp {
                        op: AstOp::Less,
                        lhs: 0,
                        rhs: 1,
                    },
                    Id(2),
                    Id(1),
                    Call {
                        function: 3,
                        args: vec![4],
                    }, // 5
                    Until {
                        condition: 2,
                        body: 5,
                    },
                    MainBlock {
                        body: vec![6],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y"), Constant::Str("f")]),
            )
        }

        #[test]
        fn list_comprehension_for() {
            let source = "[x y for x in 0..1]";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Number0,
                    Number1,
                    Range {
                        start: 2,
                        end: 3,
                        inclusive: false,
                    },
                    Call {
                        function: 0,
                        args: vec![1],
                    }, // 5
                    For(AstFor {
                        args: vec![0],
                        ranges: vec![4],
                        condition: None,
                        body: 5,
                    }),
                    List(vec![6]),
                    MainBlock {
                        body: vec![7],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y")]),
            )
        }

        #[test]
        fn list_comprehension_while() {
            let source = "[f x while (f y) < 10]";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Id(0),
                    Id(2),
                    Call {
                        function: 2,
                        args: vec![3],
                    },
                    Number(3), // 5
                    BinaryOp {
                        op: AstOp::Less,
                        lhs: 4,
                        rhs: 5,
                    },
                    Call {
                        function: 0,
                        args: vec![1],
                    },
                    While {
                        condition: 6,
                        body: 7,
                    },
                    List(vec![8]),
                    MainBlock {
                        body: vec![9],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("f"),
                    Constant::Str("x"),
                    Constant::Str("y"),
                    Constant::Number(10.0),
                ]),
            )
        }

        #[test]
        fn list_comprehension_until() {
            let source = "[f x until (f y) >= 10]";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Id(0),
                    Id(2),
                    Call {
                        function: 2,
                        args: vec![3],
                    },
                    Number(3), // 5
                    BinaryOp {
                        op: AstOp::GreaterOrEqual,
                        lhs: 4,
                        rhs: 5,
                    },
                    Call {
                        function: 0,
                        args: vec![1],
                    },
                    Until {
                        condition: 6,
                        body: 7,
                    },
                    List(vec![8]),
                    MainBlock {
                        body: vec![9],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("f"),
                    Constant::Str("x"),
                    Constant::Str("y"),
                    Constant::Number(10.0),
                ]),
            )
        }
    }

    mod functions {
        use super::*;
        use crate::node::{AssignTarget, Scope};

        #[test]
        fn inline_no_args() {
            let source = "
a = || 42
a()";
            check_ast(
                source,
                &[
                    Id(0),
                    Number(1),
                    Function(Function {
                        args: vec![],
                        local_count: 0,
                        accessed_non_locals: vec![],
                        body: 1,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 2,
                    },
                    Lookup(vec![LookupNode::Id(0), LookupNode::Call(vec![])]),
                    MainBlock {
                        body: vec![3, 4],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("a"), Constant::Number(42.0)]),
            )
        }

        #[test]
        fn inline_two_args() {
            let source = "|x y| x + y";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 0,
                        rhs: 1,
                    },
                    Function(Function {
                        args: vec![0, 1],
                        local_count: 2,
                        accessed_non_locals: vec![],
                        body: 2,
                        is_instance_function: false,
                    }),
                    MainBlock {
                        body: vec![3],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("y")]),
            )
        }

        #[test]
        fn with_body() {
            let source = "\
f = |x|
  y = x
  y = y + 1
  y
f 42";
            check_ast(
                source,
                &[
                    Id(0), // f
                    Id(2), // y
                    Id(1), // x
                    Assign {
                        target: AssignTarget {
                            target_index: 1,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 2,
                    },
                    Id(2), // y
                    Id(2), // y // 5
                    Number1,
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 5,
                        rhs: 6,
                    },
                    Assign {
                        target: AssignTarget {
                            target_index: 4,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 7,
                    },
                    Id(2),                // y
                    Block(vec![3, 8, 9]), // 10
                    Function(Function {
                        args: vec![1],
                        local_count: 2,
                        accessed_non_locals: vec![],
                        body: 10,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 11,
                    },
                    Id(0),
                    Number(3),
                    Call {
                        function: 13,
                        args: vec![14],
                    }, // 15
                    MainBlock {
                        body: vec![12, 15],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("f"),
                    Constant::Str("x"),
                    Constant::Str("y"),
                    Constant::Number(42.0),
                ]),
            )
        }

        #[test]
        fn with_body_nested() {
            let source = "\
f = |x|
  y = |z|
    z
  y x
f 42";
            check_ast(
                source,
                &[
                    Id(0), // f
                    Id(2), // y
                    Id(3), // z
                    Function(Function {
                        args: vec![3],
                        local_count: 1,
                        accessed_non_locals: vec![],
                        body: 2,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 1,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 3,
                    },
                    Id(2), // y // 5
                    Id(1), // x
                    Call {
                        function: 5,
                        args: vec![6],
                    },
                    Block(vec![4, 7]),
                    Function(Function {
                        args: vec![1],
                        local_count: 2,
                        accessed_non_locals: vec![],
                        body: 8,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 9,
                    }, // 10
                    Id(0), // f
                    Number(4),
                    Call {
                        function: 11,
                        args: vec![12],
                    },
                    MainBlock {
                        body: vec![10, 13],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("f"),
                    Constant::Str("x"),
                    Constant::Str("y"),
                    Constant::Str("z"),
                    Constant::Number(42.0),
                ]),
            )
        }

        #[test]
        fn call_negative_arg() {
            let source = "\
f 0 -x";
            check_ast(
                source,
                &[
                    Id(0),
                    Number0,
                    Id(1),
                    Negate(2),
                    Call {
                        function: 0,
                        args: vec![1, 3],
                    },
                    MainBlock {
                        body: vec![4],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("f"), Constant::Str("x")]),
            )
        }

        #[test]
        fn instance_function() {
            let source = "{foo: 42, bar: |self x| self.foo = x}";
            check_ast(
                source,
                &[
                    Number(1),
                    Lookup(vec![LookupNode::Id(3), LookupNode::Id(0)]),
                    Id(4), // x
                    Assign {
                        target: AssignTarget {
                            target_index: 1,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 2,
                    },
                    Function(Function {
                        args: vec![3, 4],
                        local_count: 2,
                        accessed_non_locals: vec![],
                        body: 3,
                        is_instance_function: true,
                    }),
                    Map(vec![(0, 0), (2, 4)]), // Map entries are constant/ast index pairs
                    MainBlock {
                        body: vec![5],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("foo"),
                    Constant::Number(42.0),
                    Constant::Str("bar"),
                    Constant::Str("self"),
                    Constant::Str("x"),
                ]),
            )
        }

        #[test]
        fn instance_function_block() {
            let source = "
f = ||
  foo: 42
  bar: |self x| self.foo = x
f()";
            check_ast(
                source,
                &[
                    Id(0),
                    Number(2),
                    Lookup(vec![LookupNode::Id(4), LookupNode::Id(1)]),
                    Id(5), // x
                    Assign {
                        target: AssignTarget {
                            target_index: 2,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 3,
                    },
                    Function(Function {
                        args: vec![4, 5],
                        local_count: 2,
                        accessed_non_locals: vec![],
                        body: 4,
                        is_instance_function: true,
                    }), // 5
                    Map(vec![(1, 1), (3, 5)]), // Map entries are constant/ast index pairs
                    Function(Function {
                        args: vec![],
                        local_count: 0,
                        accessed_non_locals: vec![],
                        body: 6,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 7,
                    },
                    Lookup(vec![LookupNode::Id(0), LookupNode::Call(vec![])]),
                    MainBlock {
                        body: vec![8, 9],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("f"),
                    Constant::Str("foo"),
                    Constant::Number(42.0),
                    Constant::Str("bar"),
                    Constant::Str("self"),
                    Constant::Str("x"),
                ]),
            )
        }

        #[test]
        fn nested_function_with_loops_and_ifs() {
            let source = "\
f = |n|
  f2 = |n|
    for i in 0..1
      if i == n
        return i

  for x in 0..1
    if x == n
      return f2 n
f 1
";
            check_ast(
                source,
                &[
                    Id(0), // f
                    Id(2), // f2
                    Number0,
                    Number1,
                    Range {
                        start: 2,
                        end: 3,
                        inclusive: false,
                    },
                    Id(3), // 5 - i
                    Id(1), // n
                    BinaryOp {
                        op: AstOp::Equal,
                        lhs: 5,
                        rhs: 6,
                    },
                    Id(3),
                    ReturnExpression(8),
                    If(AstIf {
                        condition: 7,
                        then_node: 9,
                        else_if_blocks: vec![],
                        else_node: None,
                    }), // 10
                    For(AstFor {
                        args: vec![3],
                        ranges: vec![4],
                        condition: None,
                        body: 10,
                    }),
                    Function(Function {
                        args: vec![1],
                        local_count: 2,
                        accessed_non_locals: vec![],
                        body: 11,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 1,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 12,
                    },
                    Number0,
                    Number1, // 15
                    Range {
                        start: 14,
                        end: 15,
                        inclusive: false,
                    },
                    Id(4), // x
                    Id(1), // n
                    BinaryOp {
                        op: AstOp::Equal,
                        lhs: 17,
                        rhs: 18,
                    },
                    Id(2), // 20 - f2
                    Id(1), // n
                    Call {
                        function: 20,
                        args: vec![21],
                    },
                    ReturnExpression(22),
                    If(AstIf {
                        condition: 19,
                        then_node: 23,
                        else_if_blocks: vec![],
                        else_node: None,
                    }),
                    For(AstFor {
                        args: vec![4], // x
                        ranges: vec![16],
                        condition: None,
                        body: 24,
                    }), // 25
                    Block(vec![13, 25]),
                    Function(Function {
                        args: vec![1],
                        local_count: 3,
                        accessed_non_locals: vec![],
                        body: 26,
                        is_instance_function: false,
                    }),
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 27,
                    },
                    Id(0),   // f
                    Number1, // 30
                    Call {
                        function: 29,
                        args: vec![30],
                    },
                    MainBlock {
                        body: vec![28, 31],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("f"),
                    Constant::Str("n"),
                    Constant::Str("f2"),
                    Constant::Str("i"),
                    Constant::Str("x"),
                ]),
            )
        }

        #[test]
        fn non_local_access() {
            let source = "|| x += 1";
            check_ast(
                source,
                &[
                    Id(0),
                    Number1,
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Add,
                        expression: 1,
                    },
                    Function(Function {
                        args: vec![],
                        local_count: 0,
                        accessed_non_locals: vec![0], // initial read of x via capture
                        body: 2,
                        is_instance_function: false,
                    }),
                    MainBlock {
                        body: vec![3],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x")]),
            )
        }

        #[test]
        fn call_with_functor() {
            let source = "\
z = y [0..20] |x| x > 1
y z";
            check_ast(
                source,
                &[
                    Id(0),
                    Id(1),
                    Number0,
                    Number(2),
                    Range {
                        start: 2,
                        end: 3,
                        inclusive: false,
                    },
                    List(vec![4]), // 5
                    Id(3),
                    Number1,
                    BinaryOp {
                        op: AstOp::Greater,
                        lhs: 6,
                        rhs: 7,
                    },
                    Function(Function {
                        args: vec![3],
                        local_count: 1,
                        accessed_non_locals: vec![],
                        body: 8,
                        is_instance_function: false,
                    }),
                    Call {
                        function: 1,
                        args: vec![5, 9],
                    }, // 10
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 10,
                    },
                    Id(1),
                    Id(0),
                    Call {
                        function: 12,
                        args: vec![13],
                    },
                    MainBlock {
                        body: vec![11, 14],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("z"),
                    Constant::Str("y"),
                    Constant::Number(20.0),
                    Constant::Str("x"),
                ]),
            )
        }
    }

    mod lookups {
        use super::*;

        #[test]
        fn array_indexing() {
            let source = "\
a[0] = a[1]
x[..]
y[..3]
z[10..][0]";
            check_ast(
                source,
                &[
                    Number0,
                    Lookup(vec![LookupNode::Id(0), LookupNode::Index(0)]),
                    Number1,
                    Lookup(vec![LookupNode::Id(0), LookupNode::Index(2)]),
                    Assign {
                        target: AssignTarget {
                            target_index: 1,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 3,
                    },
                    RangeFull, // 5
                    Lookup(vec![LookupNode::Id(1), LookupNode::Index(5)]),
                    Number(3),
                    RangeTo {
                        end: 7,
                        inclusive: false,
                    },
                    Lookup(vec![LookupNode::Id(2), LookupNode::Index(8)]),
                    Number(5), // 10
                    RangeFrom { start: 10 },
                    Number0,
                    Lookup(vec![
                        LookupNode::Id(4),
                        LookupNode::Index(11),
                        LookupNode::Index(12),
                    ]),
                    MainBlock {
                        body: vec![4, 6, 9, 13],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("a"),
                    Constant::Str("x"),
                    Constant::Str("y"),
                    Constant::Number(3.0),
                    Constant::Str("z"),
                    Constant::Number(10.0),
                ]),
            )
        }

        #[test]
        fn map_lookup() {
            let source = "\
x.foo
x.bar()
x.bar().baz = 1
x.foo 42";
            check_ast(
                source,
                &[
                    Lookup(vec![LookupNode::Id(0), LookupNode::Id(1)]),
                    Lookup(vec![
                        LookupNode::Id(0),
                        LookupNode::Id(2),
                        LookupNode::Call(vec![]),
                    ]),
                    Lookup(vec![
                        LookupNode::Id(0),
                        LookupNode::Id(2),
                        LookupNode::Call(vec![]),
                        LookupNode::Id(3),
                    ]),
                    Number1,
                    Assign {
                        target: AssignTarget {
                            target_index: 2,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 3,
                    },
                    Number(4), // 5
                    Lookup(vec![
                        LookupNode::Id(0),
                        LookupNode::Id(1),
                        LookupNode::Call(vec![5]),
                    ]),
                    MainBlock {
                        body: vec![0, 1, 4, 6],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("x"),
                    Constant::Str("foo"),
                    Constant::Str("bar"),
                    Constant::Str("baz"),
                    Constant::Number(42.0),
                ]),
            )
        }

        #[test]
        fn map_lookup_in_list() {
            let source = "[m.foo m.bar]";
            check_ast(
                source,
                &[
                    Lookup(vec![LookupNode::Id(0), LookupNode::Id(1)]),
                    Lookup(vec![LookupNode::Id(0), LookupNode::Id(2)]),
                    List(vec![0, 1]),
                    MainBlock {
                        body: vec![2],
                        local_count: 0,
                    },
                ],
                Some(&[
                    Constant::Str("m"),
                    Constant::Str("foo"),
                    Constant::Str("bar"),
                ]),
            )
        }
    }

    mod keywords {
        use super::*;

        #[test]
        fn flow() {
            let source = "\
break
continue
return
return 1";
            check_ast(
                source,
                &[
                    Break,
                    Continue,
                    Return,
                    Number1,
                    ReturnExpression(3),
                    MainBlock {
                        body: vec![0, 1, 2, 4],
                        local_count: 0,
                    },
                ],
                None,
            )
        }

        #[test]
        fn expressions() {
            let source = "\
copy x
not true
debug x + x";
            check_ast(
                source,
                &[
                    Id(0),
                    CopyExpression(0),
                    BoolTrue,
                    Negate(2),
                    Id(0),
                    Id(0), // 5
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 4,
                        rhs: 5,
                    },
                    Debug {
                        expression_string: 1,
                        expression: 6,
                    },
                    MainBlock {
                        body: vec![1, 3, 7],
                        local_count: 0,
                    },
                ],
                Some(&[Constant::Str("x"), Constant::Str("x + x")]),
            )
        }
    }

    mod line_continuation {
        use super::*;

        #[test]
        fn arithmetic() {
            let source = r"
a = 1 + \
    2 + \
    3
";
            check_ast(
                source,
                &[
                    Id(0),
                    Number1,
                    Number(1),
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 1,
                        rhs: 2,
                    },
                    Number(2),
                    BinaryOp {
                        op: AstOp::Add,
                        lhs: 3,
                        rhs: 4,
                    }, // 5
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 5,
                    },
                    MainBlock {
                        body: vec![6],
                        local_count: 1,
                    },
                ],
                Some(&[
                    Constant::Str("a"),
                    Constant::Number(2.0),
                    Constant::Number(3.0),
                ]),
            )
        }
    }

    mod import {
        use super::*;

        #[test]
        fn import_module() {
            let source = "import foo";
            check_ast(
                source,
                &[
                    Import {
                        module: vec![0],
                        items: vec![],
                    },
                    MainBlock {
                        body: vec![0],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("foo")]),
            )
        }

        #[test]
        fn import_item() {
            let source = "import foo.bar";
            check_ast(
                source,
                &[
                    Import {
                        module: vec![0, 1],
                        items: vec![],
                    },
                    MainBlock {
                        body: vec![0],
                        local_count: 1,
                    },
                ],
                Some(&[Constant::Str("foo"), Constant::Str("bar")]),
            )
        }

        #[test]
        fn import_item_used_in_assignment() {
            let source = "x = import foo.bar";
            check_ast(
                source,
                &[
                    Id(0),
                    Import {
                        module: vec![1, 2],
                        items: vec![],
                    },
                    Assign {
                        target: AssignTarget {
                            target_index: 0,
                            scope: Scope::Local,
                        },
                        op: AssignOp::Equal,
                        expression: 1,
                    },
                    MainBlock {
                        body: vec![2],
                        local_count: 2, // x and bar both assigned locally
                    },
                ],
                Some(&[
                    Constant::Str("x"),
                    Constant::Str("foo"),
                    Constant::Str("bar"),
                ]),
            )
        }

        #[test]
        fn import_items() {
            let source = "import [bar baz] from foo";
            check_ast(
                source,
                &[
                    Import {
                        module: vec![2],
                        items: vec![0, 1],
                    },
                    MainBlock {
                        body: vec![0],
                        local_count: 3,
                    },
                ],
                Some(&[
                    Constant::Str("bar"),
                    Constant::Str("baz"),
                    Constant::Str("foo"),
                ]),
            )
        }

        #[test]
        fn import_items_from_child() {
            let source = "import [abc xyz] from foo.bar";
            check_ast(
                source,
                &[
                    Import {
                        module: vec![2, 3],
                        items: vec![0, 1],
                    },
                    MainBlock {
                        body: vec![0],
                        local_count: 3,
                    },
                ],
                Some(&[
                    Constant::Str("abc"),
                    Constant::Str("xyz"),
                    Constant::Str("foo"),
                    Constant::Str("bar"),
                ]),
            )
        }
    }
}
