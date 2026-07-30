use std::io::Cursor;

use brush_parser::{Parser, ParserOptions, ast, pattern, word};

use super::ShellAnalysisError;

const MAX_ANALYSIS_NODES: usize = 131_072;
const MAX_ANALYSIS_DEPTH: usize = 64;

#[derive(Debug)]
pub(super) struct ShellProgram {
    pub commands: Vec<ShellCommand>,
    pub compound_redirects: Vec<CompoundRedirects>,
    pub features: ExecutionFeatures,
}

#[derive(Debug, Default)]
pub(super) struct ExecutionFeatures {
    pub command_substitution: bool,
    pub process_substitution: bool,
    pub executable_group: bool,
}

#[derive(Debug)]
#[allow(dead_code)] // Structural context is asserted by adapter tests; policy consumes words.
pub(super) struct ShellCommand {
    pub context: ExecutionContext,
    pub assignments: Vec<ShellAssignment>,
    pub invalidated_assignments: Vec<String>,
    pub command: Option<ShellWord>,
    pub arguments: Vec<ShellWord>,
    pub redirects: Vec<ShellRedirect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExecutionContext {
    TopLevel,
    Pipeline,
    Asynchronous,
    Conditional,
    Loop,
    Group,
    Subshell,
    ProcessSubstitution,
}

fn restrict_context(current: ExecutionContext, nested: ExecutionContext) -> ExecutionContext {
    match current {
        ExecutionContext::TopLevel => nested,
        ExecutionContext::Pipeline
        | ExecutionContext::Asynchronous
        | ExecutionContext::Group
        | ExecutionContext::Subshell
        | ExecutionContext::ProcessSubstitution => current,
        ExecutionContext::Conditional | ExecutionContext::Loop => match nested {
            ExecutionContext::Pipeline
            | ExecutionContext::Asynchronous
            | ExecutionContext::Group
            | ExecutionContext::Subshell
            | ExecutionContext::ProcessSubstitution => nested,
            ExecutionContext::TopLevel | ExecutionContext::Conditional | ExecutionContext::Loop => {
                current
            }
        },
    }
}

#[derive(Debug)]
#[allow(dead_code)] // Redirect structure stays out of command/argument policy positions.
pub(super) struct CompoundRedirects {
    pub context: ExecutionContext,
    pub kind: CompoundKind,
    pub redirects: Vec<ShellRedirect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompoundKind {
    Arithmetic,
    ArithmeticForClause,
    BraceGroup,
    Subshell,
    ForClause,
    CaseClause,
    IfClause,
    WhileClause,
    UntilClause,
    Coprocess,
}

#[derive(Debug)]
pub(super) struct ShellAssignment {
    pub name: String,
    pub value: ShellWord,
    pub append: bool,
}

#[derive(Debug)]
#[allow(dead_code)] // Redirect metadata is structural and intentionally not policy input.
pub(super) struct ShellRedirect {
    pub fd: Option<i32>,
    pub kind: RedirectKind,
    pub target: Option<ShellWord>,
}

#[derive(Debug)]
#[allow(dead_code)] // Redirect kinds are retained for adapter contract tests.
pub(super) enum RedirectKind {
    Read,
    Write,
    Append,
    ReadAndWrite,
    Clobber,
    DuplicateInput,
    DuplicateOutput,
    HereDocument,
    HereString,
    OutputAndError { append: bool },
}

#[derive(Debug)]
pub(super) struct ShellWord {
    #[allow(dead_code)] // Retained for structural adapter tests, not policy decisions.
    pub raw: String,
    pub literal: Option<String>,
    pub parts: Vec<WordPart>,
    pub assign_default_invalidations: Vec<String>,
    pub can_split_fields: bool,
    pub may_mutate_shell_state: bool,
}

type ProjectedPieces = (Vec<WordPart>, Option<String>, Vec<String>);

#[derive(Debug)]
pub(super) enum WordPart {
    Literal(String),
    UnquotedLiteral(String),
    TildeHome,
    TildeOther,
    Parameter {
        value: ParameterUse,
        split_fields: bool,
    },
    Arithmetic,
    CommandSubstitution,
    ProcessSubstitution,
    AnsiCEscape,
    LocalizedText,
    PathnamePattern,
    BraceExpansion,
}

#[derive(Debug)]
pub(super) enum ParameterUse {
    Named {
        name: String,
    },
    Fallback {
        name: String,
        operator: FallbackOperator,
        test: ParameterTest,
        value: ShellWord,
    },
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FallbackOperator {
    Default,
    AssignDefault,
    Alternative,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ParameterTest {
    Unset,
    UnsetOrNull,
}

#[derive(Default)]
struct AnalysisBudget {
    visited: usize,
}

impl AnalysisBudget {
    fn visit(&mut self) -> Result<(), ShellAnalysisError> {
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(ShellAnalysisError::ResourceLimit)?;
        if self.visited > MAX_ANALYSIS_NODES {
            return Err(ShellAnalysisError::ResourceLimit);
        }
        Ok(())
    }
}

struct Analyzer {
    options: ParserOptions,
    budget: AnalysisBudget,
    nesting: usize,
    result: ShellProgram,
}

pub(super) fn analyze(input: &str) -> Result<ShellProgram, ShellAnalysisError> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(Cursor::new(input), &options);
    let program = parser
        .parse_program()
        .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?;
    let mut analyzer = Analyzer {
        options,
        budget: AnalysisBudget::default(),
        nesting: 0,
        result: ShellProgram {
            commands: Vec::new(),
            compound_redirects: Vec::new(),
            features: ExecutionFeatures::default(),
        },
    };
    analyzer.visit_program(&program)?;
    Ok(analyzer.result)
}

impl Analyzer {
    fn nested<T>(
        &mut self,
        analyze: impl FnOnce(&mut Self) -> Result<T, ShellAnalysisError>,
    ) -> Result<T, ShellAnalysisError> {
        self.budget.visit()?;
        if self.nesting >= MAX_ANALYSIS_DEPTH {
            return Err(ShellAnalysisError::ResourceLimit);
        }
        self.nesting += 1;
        let result = analyze(self);
        self.nesting -= 1;
        result
    }

    fn visit_program(&mut self, program: &ast::Program) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        for complete_command in &program.complete_commands {
            self.visit_compound_list(complete_command, ExecutionContext::TopLevel)?;
        }
        Ok(())
    }

    fn visit_compound_list(
        &mut self,
        list: &ast::CompoundList,
        context: ExecutionContext,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        for item in &list.0 {
            self.budget.visit()?;
            let item_context = if matches!(item.1, ast::SeparatorOperator::Async) {
                restrict_context(context, ExecutionContext::Asynchronous)
            } else {
                context
            };
            self.visit_and_or_list(&item.0, item_context)?;
        }
        Ok(())
    }

    fn visit_and_or_list(
        &mut self,
        list: &ast::AndOrList,
        context: ExecutionContext,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        for (index, (_, pipeline)) in list.into_iter().enumerate() {
            let pipeline_context = if index == 0 {
                context
            } else {
                restrict_context(context, ExecutionContext::Conditional)
            };
            self.visit_pipeline(pipeline, pipeline_context)?;
        }
        Ok(())
    }

    fn visit_pipeline(
        &mut self,
        pipeline: &ast::Pipeline,
        context: ExecutionContext,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        let context = if pipeline.seq.len() > 1 {
            restrict_context(context, ExecutionContext::Pipeline)
        } else {
            context
        };
        for command in &pipeline.seq {
            self.visit_command(command, context)?;
        }
        Ok(())
    }

    fn visit_command(
        &mut self,
        command: &ast::Command,
        context: ExecutionContext,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        match command {
            ast::Command::Simple(simple) => self.visit_simple_command(simple, context),
            ast::Command::Compound(compound, redirects) => {
                let redirects = redirects
                    .as_ref()
                    .map(|redirects| self.project_redirect_list(redirects))
                    .transpose()?;
                if let Some(redirects) = &redirects {
                    let invalidated_assignments = redirect_assignment_invalidations(redirects);
                    if !invalidated_assignments.is_empty() {
                        self.result.commands.push(ShellCommand {
                            context,
                            assignments: Vec::new(),
                            invalidated_assignments,
                            command: None,
                            arguments: Vec::new(),
                            redirects: Vec::new(),
                        });
                    }
                }
                if let ast::CompoundCommand::Arithmetic(arithmetic) = compound {
                    let (_, invalidated_assignments, _) =
                        self.project_arithmetic_operand(&arithmetic.expr)?;
                    let inert_command = self.push_inert_command(context);
                    self.result.commands[inert_command].invalidated_assignments =
                        invalidated_assignments;
                }
                self.visit_compound_command(compound, context)?;
                if let Some(redirects) = redirects {
                    self.result.compound_redirects.push(CompoundRedirects {
                        context,
                        kind: compound_kind(compound),
                        redirects,
                    });
                }
                Ok(())
            }
            ast::Command::Function(_) => Err(ShellAnalysisError::UnsupportedSyntax),
            ast::Command::ExtendedTest(extended_test, redirects) => {
                let invalidated_assignments = self.visit_extended_test(extended_test)?;
                let inert_command = self.push_inert_command(context);
                self.result.commands[inert_command].invalidated_assignments =
                    invalidated_assignments;
                if let Some(redirects) = redirects {
                    let redirects = self.project_redirect_list(redirects)?;
                    self.result.commands[inert_command]
                        .invalidated_assignments
                        .extend(redirect_assignment_invalidations(&redirects));
                    self.result.commands[inert_command].redirects = redirects;
                }
                Ok(())
            }
        }
    }

    fn visit_extended_test(
        &mut self,
        command: &ast::ExtendedTestExprCommand,
    ) -> Result<Vec<String>, ShellAnalysisError> {
        self.budget.visit()?;
        let mut invalidated_assignments = Vec::new();
        let mut expressions = vec![&command.expr];
        while let Some(expression) = expressions.pop() {
            self.budget.visit()?;
            match expression {
                ast::ExtendedTestExpr::And(left, right)
                | ast::ExtendedTestExpr::Or(left, right) => {
                    expressions.push(right);
                    expressions.push(left);
                }
                ast::ExtendedTestExpr::Not(expression)
                | ast::ExtendedTestExpr::Parenthesized(expression) => {
                    expressions.push(expression);
                }
                ast::ExtendedTestExpr::UnaryTest(_, word) => {
                    let word = self.project_word(word)?;
                    collect_assignment_invalidations(&word, &mut invalidated_assignments);
                }
                ast::ExtendedTestExpr::BinaryTest(_, left, right) => {
                    let left = self.project_word(left)?;
                    let right = self.project_word(right)?;
                    collect_assignment_invalidations(&left, &mut invalidated_assignments);
                    collect_assignment_invalidations(&right, &mut invalidated_assignments);
                }
            }
        }
        Ok(invalidated_assignments)
    }

    fn visit_compound_command(
        &mut self,
        command: &ast::CompoundCommand,
        context: ExecutionContext,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        match command {
            ast::CompoundCommand::Arithmetic(_) => Ok(()),
            ast::CompoundCommand::ArithmeticForClause(command) => {
                let context = restrict_context(context, ExecutionContext::Loop);
                let mut invalidated_assignments = Vec::new();
                for expression in [
                    command.initializer.as_ref(),
                    command.condition.as_ref(),
                    command.updater.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    let (_, invalidations, _) = self.project_arithmetic_operand(expression)?;
                    extend_unique(
                        &mut invalidated_assignments,
                        invalidations.iter().map(String::as_str),
                    );
                }
                if !invalidated_assignments.is_empty() {
                    self.result.commands.push(ShellCommand {
                        context,
                        assignments: Vec::new(),
                        invalidated_assignments,
                        command: None,
                        arguments: Vec::new(),
                        redirects: Vec::new(),
                    });
                }
                self.nested(|analyzer| analyzer.visit_compound_list(&command.body.list, context))
            }
            ast::CompoundCommand::BraceGroup(command) => {
                self.result.features.executable_group = true;
                let context = restrict_context(context, ExecutionContext::Group);
                self.nested(|analyzer| analyzer.visit_compound_list(&command.list, context))
            }
            ast::CompoundCommand::Subshell(command) => {
                self.result.features.executable_group = true;
                let context = restrict_context(context, ExecutionContext::Subshell);
                self.nested(|analyzer| analyzer.visit_compound_list(&command.list, context))
            }
            ast::CompoundCommand::ForClause(command) => {
                let context = restrict_context(context, ExecutionContext::Loop);
                let mut invalidated_assignments = vec![command.variable_name.clone()];
                if let Some(values) = &command.values {
                    for value in values {
                        let word = self.project_word(value)?;
                        extend_unique(
                            &mut invalidated_assignments,
                            word.assign_default_invalidations.iter().map(String::as_str),
                        );
                    }
                }
                self.result.commands.push(ShellCommand {
                    context,
                    assignments: Vec::new(),
                    invalidated_assignments,
                    command: None,
                    arguments: Vec::new(),
                    redirects: Vec::new(),
                });
                self.nested(|analyzer| analyzer.visit_compound_list(&command.body.list, context))
            }
            ast::CompoundCommand::CaseClause(_) => Err(ShellAnalysisError::UnsupportedSyntax),
            ast::CompoundCommand::IfClause(command) => {
                let context = restrict_context(context, ExecutionContext::Conditional);
                self.nested(|analyzer| analyzer.visit_compound_list(&command.condition, context))?;
                self.nested(|analyzer| analyzer.visit_compound_list(&command.then, context))?;
                if let Some(elses) = &command.elses {
                    for else_clause in elses {
                        self.budget.visit()?;
                        if let Some(condition) = &else_clause.condition {
                            self.nested(|analyzer| {
                                analyzer.visit_compound_list(condition, context)
                            })?;
                        }
                        self.nested(|analyzer| {
                            analyzer.visit_compound_list(&else_clause.body, context)
                        })?;
                    }
                }
                Ok(())
            }
            ast::CompoundCommand::WhileClause(command)
            | ast::CompoundCommand::UntilClause(command) => {
                let context = restrict_context(context, ExecutionContext::Loop);
                self.nested(|analyzer| analyzer.visit_compound_list(&command.0, context))?;
                self.nested(|analyzer| analyzer.visit_compound_list(&command.1.list, context))
            }
            ast::CompoundCommand::Coprocess(command) => {
                self.result.features.executable_group = true;
                if let Some(name) = &command.name {
                    self.project_word(name)?;
                }
                let context = restrict_context(context, ExecutionContext::Group);
                self.nested(|analyzer| analyzer.visit_command(&command.body, context))
            }
        }
    }

    fn visit_simple_command(
        &mut self,
        command: &ast::SimpleCommand,
        context: ExecutionContext,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        let mut projected = ShellCommand {
            context,
            assignments: Vec::new(),
            invalidated_assignments: Vec::new(),
            command: command
                .word_or_name
                .as_ref()
                .map(|word| self.project_word(word))
                .transpose()?,
            arguments: Vec::new(),
            redirects: Vec::new(),
        };
        if let Some(prefix) = &command.prefix {
            self.budget.visit()?;
            for item in &prefix.0 {
                self.visit_simple_item(item, &mut projected, true)?;
            }
        }
        if let Some(suffix) = &command.suffix {
            self.budget.visit()?;
            for item in &suffix.0 {
                self.visit_simple_item(item, &mut projected, false)?;
            }
        }
        if let Some(command) = &projected.command {
            collect_assignment_invalidations(command, &mut projected.invalidated_assignments);
        }
        for assignment in &projected.assignments {
            collect_assignment_invalidations(
                &assignment.value,
                &mut projected.invalidated_assignments,
            );
        }
        for argument in &projected.arguments {
            collect_assignment_invalidations(argument, &mut projected.invalidated_assignments);
        }
        for redirect in &projected.redirects {
            if let Some(target) = &redirect.target {
                collect_assignment_invalidations(target, &mut projected.invalidated_assignments);
            }
        }
        self.result.commands.push(projected);
        Ok(())
    }

    fn visit_simple_item(
        &mut self,
        item: &ast::CommandPrefixOrSuffixItem,
        command: &mut ShellCommand,
        prefix: bool,
    ) -> Result<(), ShellAnalysisError> {
        self.budget.visit()?;
        match item {
            ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                command.redirects.push(self.project_redirect(redirect)?);
            }
            ast::CommandPrefixOrSuffixItem::Word(word) => {
                command.arguments.push(self.project_word(word)?);
            }
            ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                if prefix {
                    command
                        .assignments
                        .push(self.project_assignment(assignment)?);
                } else {
                    command.arguments.push(self.project_word(word)?);
                }
            }
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => {
                command
                    .arguments
                    .push(self.project_process_substitution(kind, subshell)?);
            }
        }
        Ok(())
    }

    fn project_assignment(
        &mut self,
        assignment: &ast::Assignment,
    ) -> Result<ShellAssignment, ShellAnalysisError> {
        self.budget.visit()?;
        let name = match &assignment.name {
            ast::AssignmentName::VariableName(name) => name.clone(),
            ast::AssignmentName::ArrayElementName(_, _) => {
                return Err(ShellAnalysisError::UnsupportedSyntax);
            }
        };
        let value = match &assignment.value {
            ast::AssignmentValue::Scalar(value) => self.project_word(value)?,
            ast::AssignmentValue::Array(_) => {
                return Err(ShellAnalysisError::UnsupportedSyntax);
            }
        };
        Ok(ShellAssignment {
            name,
            value,
            append: assignment.append,
        })
    }

    fn push_inert_command(&mut self, context: ExecutionContext) -> usize {
        let index = self.result.commands.len();
        self.result.commands.push(ShellCommand {
            context,
            assignments: Vec::new(),
            invalidated_assignments: Vec::new(),
            command: None,
            arguments: Vec::new(),
            redirects: Vec::new(),
        });
        index
    }

    fn project_redirect_list(
        &mut self,
        redirects: &ast::RedirectList,
    ) -> Result<Vec<ShellRedirect>, ShellAnalysisError> {
        self.budget.visit()?;
        let mut projected = Vec::with_capacity(redirects.0.len());
        for redirect in &redirects.0 {
            projected.push(self.project_redirect(redirect)?);
        }
        Ok(projected)
    }

    fn project_redirect(
        &mut self,
        redirect: &ast::IoRedirect,
    ) -> Result<ShellRedirect, ShellAnalysisError> {
        self.budget.visit()?;
        match redirect {
            ast::IoRedirect::File(fd, kind, target) => {
                let kind = match kind {
                    ast::IoFileRedirectKind::Read => RedirectKind::Read,
                    ast::IoFileRedirectKind::Write => RedirectKind::Write,
                    ast::IoFileRedirectKind::Append => RedirectKind::Append,
                    ast::IoFileRedirectKind::ReadAndWrite => RedirectKind::ReadAndWrite,
                    ast::IoFileRedirectKind::Clobber => RedirectKind::Clobber,
                    ast::IoFileRedirectKind::DuplicateInput => RedirectKind::DuplicateInput,
                    ast::IoFileRedirectKind::DuplicateOutput => RedirectKind::DuplicateOutput,
                };
                let target = self.project_redirect_target(target)?;
                Ok(ShellRedirect {
                    fd: *fd,
                    kind,
                    target,
                })
            }
            ast::IoRedirect::HereDocument(fd, document) => {
                self.project_word(&document.here_end)?;
                let target = if document.requires_expansion {
                    Some(self.project_heredoc_word(&document.doc)?)
                } else {
                    Some(self.project_literal_word(&document.doc.value)?)
                };
                Ok(ShellRedirect {
                    fd: *fd,
                    kind: RedirectKind::HereDocument,
                    target,
                })
            }
            ast::IoRedirect::HereString(fd, word) => Ok(ShellRedirect {
                fd: *fd,
                kind: RedirectKind::HereString,
                target: Some(self.project_word(word)?),
            }),
            ast::IoRedirect::OutputAndError(word, append) => Ok(ShellRedirect {
                fd: None,
                kind: RedirectKind::OutputAndError { append: *append },
                target: Some(self.project_word(word)?),
            }),
        }
    }

    fn project_redirect_target(
        &mut self,
        target: &ast::IoFileRedirectTarget,
    ) -> Result<Option<ShellWord>, ShellAnalysisError> {
        self.budget.visit()?;
        match target {
            ast::IoFileRedirectTarget::Filename(word)
            | ast::IoFileRedirectTarget::Duplicate(word) => self.project_word(word).map(Some),
            ast::IoFileRedirectTarget::Fd(fd) => {
                let raw = fd.to_string();
                Ok(Some(ShellWord {
                    raw: raw.clone(),
                    literal: Some(raw.clone()),
                    parts: vec![WordPart::Literal(raw)],
                    assign_default_invalidations: Vec::new(),
                    can_split_fields: false,
                    may_mutate_shell_state: false,
                }))
            }
            ast::IoFileRedirectTarget::ProcessSubstitution(kind, command) => {
                self.project_process_substitution(kind, command).map(Some)
            }
        }
    }

    fn project_process_substitution(
        &mut self,
        kind: &ast::ProcessSubstitutionKind,
        command: &ast::SubshellCommand,
    ) -> Result<ShellWord, ShellAnalysisError> {
        self.result.features.process_substitution = true;
        self.budget.visit()?;
        self.budget.visit()?;
        let raw = match kind {
            ast::ProcessSubstitutionKind::Read => "<(...)",
            ast::ProcessSubstitutionKind::Write => ">(...)",
        }
        .into();
        self.nested(|analyzer| {
            analyzer.visit_compound_list(&command.list, ExecutionContext::ProcessSubstitution)
        })?;
        Ok(ShellWord {
            raw,
            literal: None,
            parts: vec![WordPart::ProcessSubstitution],
            assign_default_invalidations: Vec::new(),
            can_split_fields: false,
            may_mutate_shell_state: false,
        })
    }

    fn project_word(&mut self, word: &ast::Word) -> Result<ShellWord, ShellAnalysisError> {
        self.project_raw_word(&word.value, false)
    }

    fn project_heredoc_word(&mut self, word: &ast::Word) -> Result<ShellWord, ShellAnalysisError> {
        self.project_raw_word(&word.value, true)
    }

    fn project_literal_word(&mut self, raw: &str) -> Result<ShellWord, ShellAnalysisError> {
        self.budget.visit()?;
        self.budget.visit()?;
        Ok(ShellWord {
            raw: raw.into(),
            literal: Some(raw.into()),
            parts: vec![WordPart::Literal(raw.into())],
            assign_default_invalidations: Vec::new(),
            can_split_fields: false,
            may_mutate_shell_state: false,
        })
    }

    fn project_raw_word(
        &mut self,
        raw: &str,
        heredoc: bool,
    ) -> Result<ShellWord, ShellAnalysisError> {
        self.budget.visit()?;
        let pieces = if heredoc {
            word::parse_heredoc(raw, &self.options)
        } else {
            word::parse(raw, &self.options)
        }
        .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?;
        let (parts, mut literal, assign_default_invalidations) =
            self.project_pieces(&pieces, heredoc)?;
        let mut parts = parts;
        if !heredoc {
            if let Some(classification) = quote_aware_word_syntax(&pieces) {
                let has_brace_expansion =
                    word::parse_brace_expansions(&classification, &self.options)
                        .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?
                        .is_some_and(|pieces| {
                            pieces
                                .iter()
                                .any(|piece| matches!(piece, word::BraceExpressionOrText::Expr(_)))
                        });
                let has_pathname_pattern = has_active_pathname_pattern(&classification);
                if has_brace_expansion
                    && !parts
                        .iter()
                        .any(|part| matches!(part, WordPart::BraceExpansion))
                {
                    literal = None;
                    parts.push(WordPart::BraceExpansion);
                }
                if has_pathname_pattern
                    && !parts
                        .iter()
                        .any(|part| matches!(part, WordPart::PathnamePattern))
                {
                    literal = None;
                    parts.push(WordPart::PathnamePattern);
                }
            }
        }
        if raw == "["
            && matches!(
                pieces.as_slice(),
                [word::WordPieceWithSource {
                    piece: word::WordPiece::Text(text),
                    ..
                }] if text == "["
            )
        {
            literal = Some(raw.into());
        }
        Ok(ShellWord {
            raw: raw.into(),
            literal,
            may_mutate_shell_state: word_parts_may_mutate_shell_state(&parts),
            parts,
            assign_default_invalidations,
            can_split_fields: !heredoc && pieces_have_unquoted_parameter_expansion(&pieces, false),
        })
    }

    fn project_pieces(
        &mut self,
        pieces: &[word::WordPieceWithSource],
        quoted: bool,
    ) -> Result<ProjectedPieces, ShellAnalysisError> {
        let mut parts = Vec::new();
        let mut literal = Some(String::new());
        let mut assign_default_invalidations = Vec::new();
        for piece in pieces {
            self.budget.visit()?;
            self.project_piece(
                &piece.piece,
                quoted,
                &mut parts,
                &mut literal,
                &mut assign_default_invalidations,
            )?;
        }
        Ok((parts, literal, assign_default_invalidations))
    }

    fn project_piece(
        &mut self,
        piece: &word::WordPiece,
        quoted: bool,
        parts: &mut Vec<WordPart>,
        literal: &mut Option<String>,
        assign_default_invalidations: &mut Vec<String>,
    ) -> Result<(), ShellAnalysisError> {
        match piece {
            word::WordPiece::Text(text) => {
                let has_brace_expansion = if quoted {
                    false
                } else {
                    word::parse_brace_expansions(text, &self.options)
                        .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?
                        .is_some_and(|pieces| {
                            pieces
                                .iter()
                                .any(|piece| matches!(piece, word::BraceExpressionOrText::Expr(_)))
                        })
                };
                let has_pathname_pattern = !quoted && has_active_pathname_pattern(text);

                if !has_brace_expansion && !has_pathname_pattern {
                    if let Some(literal) = literal {
                        literal.push_str(text);
                    }
                } else {
                    *literal = None;
                }
                if !has_brace_expansion {
                    parts.push(if quoted {
                        WordPart::Literal(text.clone())
                    } else {
                        WordPart::UnquotedLiteral(text.clone())
                    });
                }
                if has_brace_expansion {
                    parts.push(WordPart::BraceExpansion);
                }
                if has_pathname_pattern {
                    parts.push(WordPart::PathnamePattern);
                }
            }
            word::WordPiece::SingleQuotedText(text) => {
                if let Some(literal) = literal {
                    literal.push_str(text);
                }
                parts.push(WordPart::Literal(text.clone()));
            }
            word::WordPiece::AnsiCQuotedText(text) => {
                if text.contains('\\') {
                    *literal = None;
                    parts.push(WordPart::AnsiCEscape);
                } else {
                    if let Some(literal) = literal {
                        literal.push_str(text);
                    }
                    parts.push(WordPart::Literal(text.clone()));
                }
            }
            word::WordPiece::DoubleQuotedSequence(pieces) => {
                let (nested_parts, nested_literal, nested_invalidations) =
                    self.nested(|analyzer| analyzer.project_pieces(pieces, true))?;
                parts.extend(nested_parts);
                extend_unique(
                    assign_default_invalidations,
                    nested_invalidations.iter().map(String::as_str),
                );
                match (literal.as_mut(), nested_literal) {
                    (Some(literal), Some(nested)) => literal.push_str(&nested),
                    _ => *literal = None,
                }
            }
            word::WordPiece::GettextDoubleQuotedSequence(pieces) => {
                *literal = None;
                parts.push(WordPart::LocalizedText);
                let (nested_parts, _, nested_invalidations) =
                    self.nested(|analyzer| analyzer.project_pieces(pieces, true))?;
                extend_unique(
                    assign_default_invalidations,
                    nested_invalidations.iter().map(String::as_str),
                );
                parts.extend(nested_parts.into_iter().filter(|part| {
                    !matches!(part, WordPart::Literal(_) | WordPart::UnquotedLiteral(_))
                }));
            }
            word::WordPiece::TildeExpansion(
                word::TildeExpr::Home | word::TildeExpr::UserHome(_),
            ) => {
                *literal = None;
                parts.push(WordPart::TildeHome);
            }
            word::WordPiece::TildeExpansion(_) => {
                *literal = None;
                parts.push(WordPart::TildeOther);
            }
            word::WordPiece::ParameterExpansion(parameter) => {
                *literal = None;
                let (parameter, has_command_substitution, invalidations, may_mutate_shell_state) =
                    self.project_parameter(parameter)?;
                extend_unique(
                    assign_default_invalidations,
                    invalidations.iter().map(String::as_str),
                );
                parts.push(WordPart::Parameter {
                    value: parameter,
                    split_fields: !quoted,
                });
                if has_command_substitution {
                    parts.push(WordPart::CommandSubstitution);
                }
                if may_mutate_shell_state {
                    parts.push(WordPart::Arithmetic);
                }
            }
            word::WordPiece::CommandSubstitution(_)
            | word::WordPiece::BackquotedCommandSubstitution(_) => {
                *literal = None;
                self.result.features.command_substitution = true;
                parts.push(WordPart::CommandSubstitution);
            }
            word::WordPiece::EscapeSequence(escape) => {
                let escaped = escape
                    .strip_prefix('\\')
                    .filter(|escaped| !escaped.is_empty())
                    .ok_or(ShellAnalysisError::UnsupportedSyntax)?;
                if let Some(literal) = literal {
                    literal.push_str(escaped);
                }
                parts.push(WordPart::Literal(escaped.into()));
            }
            word::WordPiece::ArithmeticExpression(expression) => {
                *literal = None;
                parts.push(WordPart::Arithmetic);
                let (has_command_substitution, invalidations, _) =
                    self.project_arithmetic_operand(expression)?;
                extend_unique(
                    assign_default_invalidations,
                    invalidations.iter().map(String::as_str),
                );
                if has_command_substitution {
                    parts.push(WordPart::CommandSubstitution);
                }
            }
        }
        Ok(())
    }

    fn project_parameter(
        &mut self,
        expression: &word::ParameterExpr,
    ) -> Result<(ParameterUse, bool, Vec<String>, bool), ShellAnalysisError> {
        self.budget.visit()?;
        match expression {
            word::ParameterExpr::Parameter {
                parameter,
                indirect,
            } => {
                let (has_command, invalidations, may_mutate_shell_state) =
                    self.project_parameter_operand(parameter)?;
                Ok((
                    match (parameter, indirect) {
                        (word::Parameter::Named(name), false) => {
                            ParameterUse::Named { name: name.clone() }
                        }
                        _ => ParameterUse::Other,
                    },
                    has_command,
                    invalidations,
                    may_mutate_shell_state,
                ))
            }
            word::ParameterExpr::UseDefaultValues {
                parameter,
                indirect,
                test_type,
                default_value,
            } => self.project_fallback_parameter(
                parameter,
                *indirect,
                FallbackOperator::Default,
                test_type,
                default_value.as_deref(),
            ),
            word::ParameterExpr::AssignDefaultValues {
                parameter,
                indirect,
                test_type,
                default_value,
            } => self.project_fallback_parameter(
                parameter,
                *indirect,
                FallbackOperator::AssignDefault,
                test_type,
                default_value.as_deref(),
            ),
            word::ParameterExpr::UseAlternativeValue {
                parameter,
                indirect,
                test_type,
                alternative_value,
            } => self.project_fallback_parameter(
                parameter,
                *indirect,
                FallbackOperator::Alternative,
                test_type,
                alternative_value.as_deref(),
            ),
            word::ParameterExpr::IndicateErrorIfNullOrUnset {
                parameter,
                error_message,
                ..
            } => {
                let (mut has_command, mut invalidations, mut may_mutate_shell_state) =
                    self.project_parameter_operand(parameter)?;
                merge_projection_effects(
                    &mut has_command,
                    &mut invalidations,
                    &mut may_mutate_shell_state,
                    self.project_optional_word_operand(error_message.as_deref())?,
                );
                Ok((
                    ParameterUse::Other,
                    has_command,
                    invalidations,
                    may_mutate_shell_state,
                ))
            }
            word::ParameterExpr::ParameterLength { parameter, .. }
            | word::ParameterExpr::Transform { parameter, .. } => {
                let (has_command, invalidations, may_mutate_shell_state) =
                    self.project_parameter_operand(parameter)?;
                Ok((
                    ParameterUse::Other,
                    has_command,
                    invalidations,
                    may_mutate_shell_state,
                ))
            }
            word::ParameterExpr::RemoveSmallestSuffixPattern {
                parameter, pattern, ..
            }
            | word::ParameterExpr::RemoveLargestSuffixPattern {
                parameter, pattern, ..
            }
            | word::ParameterExpr::RemoveSmallestPrefixPattern {
                parameter, pattern, ..
            }
            | word::ParameterExpr::RemoveLargestPrefixPattern {
                parameter, pattern, ..
            }
            | word::ParameterExpr::UppercaseFirstChar {
                parameter, pattern, ..
            }
            | word::ParameterExpr::UppercasePattern {
                parameter, pattern, ..
            }
            | word::ParameterExpr::LowercaseFirstChar {
                parameter, pattern, ..
            }
            | word::ParameterExpr::LowercasePattern {
                parameter, pattern, ..
            } => {
                let (mut has_command, mut invalidations, mut may_mutate_shell_state) =
                    self.project_parameter_operand(parameter)?;
                merge_projection_effects(
                    &mut has_command,
                    &mut invalidations,
                    &mut may_mutate_shell_state,
                    self.project_optional_word_operand(pattern.as_deref())?,
                );
                Ok((
                    ParameterUse::Other,
                    has_command,
                    invalidations,
                    may_mutate_shell_state,
                ))
            }
            word::ParameterExpr::Substring {
                parameter,
                offset,
                length,
                ..
            } => {
                let (mut has_command, mut invalidations, mut may_mutate_shell_state) =
                    self.project_parameter_operand(parameter)?;
                merge_projection_effects(
                    &mut has_command,
                    &mut invalidations,
                    &mut may_mutate_shell_state,
                    self.project_arithmetic_operand(offset)?,
                );
                if let Some(length) = length {
                    merge_projection_effects(
                        &mut has_command,
                        &mut invalidations,
                        &mut may_mutate_shell_state,
                        self.project_arithmetic_operand(length)?,
                    );
                }
                Ok((
                    ParameterUse::Other,
                    has_command,
                    invalidations,
                    may_mutate_shell_state,
                ))
            }
            word::ParameterExpr::ReplaceSubstring {
                parameter,
                pattern,
                replacement,
                ..
            } => {
                let (mut has_command, mut invalidations, mut may_mutate_shell_state) =
                    self.project_parameter_operand(parameter)?;
                let pattern = self.project_word_operand(pattern)?;
                merge_projection_effects(
                    &mut has_command,
                    &mut invalidations,
                    &mut may_mutate_shell_state,
                    (
                        word_parts_have_command_substitution(&pattern.parts),
                        pattern.assign_default_invalidations,
                        pattern.may_mutate_shell_state,
                    ),
                );
                merge_projection_effects(
                    &mut has_command,
                    &mut invalidations,
                    &mut may_mutate_shell_state,
                    self.project_optional_word_operand(replacement.as_deref())?,
                );
                Ok((
                    ParameterUse::Other,
                    has_command,
                    invalidations,
                    may_mutate_shell_state,
                ))
            }
            word::ParameterExpr::VariableNames { .. } | word::ParameterExpr::MemberKeys { .. } => {
                Ok((ParameterUse::Other, false, Vec::new(), false))
            }
        }
    }

    fn project_fallback_parameter(
        &mut self,
        parameter: &word::Parameter,
        indirect: bool,
        operator: FallbackOperator,
        test_type: &word::ParameterTestType,
        value: Option<&str>,
    ) -> Result<(ParameterUse, bool, Vec<String>, bool), ShellAnalysisError> {
        let (has_parameter_command, mut invalidations, parameter_may_mutate_shell_state) =
            self.project_parameter_operand(parameter)?;
        let value = self.project_word_operand(value.unwrap_or(""))?;
        let has_command =
            has_parameter_command || word_parts_have_command_substitution(&value.parts);
        let may_mutate_shell_state =
            parameter_may_mutate_shell_state || value.may_mutate_shell_state;
        extend_unique(
            &mut invalidations,
            value
                .assign_default_invalidations
                .iter()
                .map(String::as_str),
        );
        let direct_name = match (parameter, indirect) {
            (word::Parameter::Named(name), false) => Some(name),
            _ => None,
        };
        if operator == FallbackOperator::AssignDefault {
            let name = direct_name.ok_or(ShellAnalysisError::UnsupportedSyntax)?;
            extend_unique(&mut invalidations, std::iter::once(name.as_str()));
        }
        Ok((
            match direct_name {
                Some(name) => ParameterUse::Fallback {
                    name: name.clone(),
                    operator,
                    test: match test_type {
                        word::ParameterTestType::Unset => ParameterTest::Unset,
                        word::ParameterTestType::UnsetOrNull => ParameterTest::UnsetOrNull,
                    },
                    value,
                },
                None => ParameterUse::Other,
            },
            has_command,
            invalidations,
            may_mutate_shell_state,
        ))
    }

    fn project_parameter_operand(
        &mut self,
        parameter: &word::Parameter,
    ) -> Result<(bool, Vec<String>, bool), ShellAnalysisError> {
        match parameter {
            word::Parameter::NamedWithIndex { index, .. } => self.project_arithmetic_value(index),
            word::Parameter::Positional(_)
            | word::Parameter::Special(_)
            | word::Parameter::Named(_)
            | word::Parameter::NamedWithAllIndices { .. } => Ok((false, Vec::new(), false)),
        }
    }

    fn project_optional_word_operand(
        &mut self,
        operand: Option<&str>,
    ) -> Result<(bool, Vec<String>, bool), ShellAnalysisError> {
        match operand {
            Some(operand) => {
                let word = self.project_word_operand(operand)?;
                Ok((
                    word_parts_have_command_substitution(&word.parts),
                    word.assign_default_invalidations,
                    word.may_mutate_shell_state,
                ))
            }
            None => Ok((false, Vec::new(), false)),
        }
    }

    fn project_word_operand(&mut self, operand: &str) -> Result<ShellWord, ShellAnalysisError> {
        self.nested(|analyzer| analyzer.project_raw_word(operand, false))
    }

    fn project_arithmetic_operand(
        &mut self,
        expression: &ast::UnexpandedArithmeticExpr,
    ) -> Result<(bool, Vec<String>, bool), ShellAnalysisError> {
        self.project_arithmetic_value(&expression.value)
    }

    fn project_arithmetic_value(
        &mut self,
        expression: &str,
    ) -> Result<(bool, Vec<String>, bool), ShellAnalysisError> {
        let (nested_parts, _, invalidations) = self.nested(|analyzer| {
            let nested_pieces = word::parse(expression, &analyzer.options)
                .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?;
            analyzer.project_pieces(&nested_pieces, true)
        })?;
        Ok((
            word_parts_have_command_substitution(&nested_parts),
            invalidations,
            true,
        ))
    }
}

fn quote_aware_word_syntax(pieces: &[word::WordPieceWithSource]) -> Option<String> {
    fn append(
        pieces: &[word::WordPieceWithSource],
        quoted: bool,
        syntax: &mut String,
    ) -> Option<()> {
        for piece in pieces {
            match &piece.piece {
                word::WordPiece::Text(text) if !quoted => syntax.push_str(text),
                word::WordPiece::Text(text) | word::WordPiece::SingleQuotedText(text) => {
                    append_quoted_text(syntax, text);
                }
                word::WordPiece::AnsiCQuotedText(text) => {
                    if text.contains('\\') {
                        return None;
                    }
                    append_quoted_text(syntax, text);
                }
                word::WordPiece::DoubleQuotedSequence(nested) => {
                    append(nested, true, syntax)?;
                }
                word::WordPiece::EscapeSequence(escape) => {
                    let escaped = escape.strip_prefix('\\')?;
                    append_quoted_text(syntax, escaped);
                }
                word::WordPiece::GettextDoubleQuotedSequence(_)
                | word::WordPiece::TildeExpansion(_)
                | word::WordPiece::ParameterExpansion(_)
                | word::WordPiece::CommandSubstitution(_)
                | word::WordPiece::BackquotedCommandSubstitution(_)
                | word::WordPiece::ArithmeticExpression(_) => return None,
            }
        }
        Some(())
    }

    let mut syntax = String::new();
    append(pieces, false, &mut syntax)?;
    Some(syntax)
}

fn append_quoted_text(syntax: &mut String, text: &str) {
    for character in text.chars() {
        syntax.push('\\');
        syntax.push(character);
    }
}

fn pieces_have_unquoted_parameter_expansion(
    pieces: &[word::WordPieceWithSource],
    quoted: bool,
) -> bool {
    pieces.iter().any(|piece| match &piece.piece {
        word::WordPiece::ParameterExpansion(_) => !quoted,
        word::WordPiece::DoubleQuotedSequence(nested)
        | word::WordPiece::GettextDoubleQuotedSequence(nested) => {
            pieces_have_unquoted_parameter_expansion(nested, true)
        }
        _ => false,
    })
}

fn word_parts_have_command_substitution(parts: &[WordPart]) -> bool {
    parts.iter().any(|part| match part {
        WordPart::CommandSubstitution => true,
        WordPart::Parameter {
            value: ParameterUse::Fallback { value, .. },
            ..
        } => word_parts_have_command_substitution(&value.parts),
        _ => false,
    })
}

fn word_parts_may_mutate_shell_state(parts: &[WordPart]) -> bool {
    parts.iter().any(|part| match part {
        WordPart::Arithmetic => true,
        WordPart::Parameter {
            value: ParameterUse::Fallback { value, .. },
            ..
        } => value.may_mutate_shell_state,
        _ => false,
    })
}

fn extend_unique<'a>(invalidated: &mut Vec<String>, names: impl IntoIterator<Item = &'a str>) {
    for name in names {
        if !invalidated.iter().any(|invalidated| invalidated == name) {
            invalidated.push(name.into());
        }
    }
}

fn merge_projection_effects(
    has_command: &mut bool,
    invalidated: &mut Vec<String>,
    may_mutate_shell_state: &mut bool,
    (nested_has_command, nested_invalidations, nested_may_mutate_shell_state): (
        bool,
        Vec<String>,
        bool,
    ),
) {
    *has_command |= nested_has_command;
    *may_mutate_shell_state |= nested_may_mutate_shell_state;
    extend_unique(invalidated, nested_invalidations.iter().map(String::as_str));
}

fn collect_assignment_invalidations(word: &ShellWord, invalidated: &mut Vec<String>) {
    extend_unique(
        invalidated,
        word.assign_default_invalidations.iter().map(String::as_str),
    );
}

fn redirect_assignment_invalidations(redirects: &[ShellRedirect]) -> Vec<String> {
    let mut invalidated = Vec::new();
    for redirect in redirects {
        if let Some(target) = &redirect.target {
            collect_assignment_invalidations(target, &mut invalidated);
        }
    }
    invalidated
}

fn has_initial_closing_bracket_pattern(text: &str) -> bool {
    let chars = text.as_bytes();
    let mut opening = 0;

    while opening < chars.len() {
        if chars[opening] == b'\\' {
            opening = opening.saturating_add(2);
            continue;
        }
        if chars[opening] != b'[' {
            opening += 1;
            continue;
        }

        let mut member = opening + 1;
        if matches!(chars.get(member), Some(b'!' | b'^')) {
            member += 1;
        }
        if chars.get(member) != Some(&b']') {
            opening += 1;
            continue;
        }

        member += 1;
        while member < chars.len() {
            if chars[member] == b'\\' {
                if member + 1 >= chars.len() {
                    break;
                }
                member += 2;
                continue;
            }
            if chars[member] == b']' {
                return true;
            }
            member += 1;
        }

        opening += 1;
    }

    false
}

pub(super) fn has_active_pathname_pattern(text: &str) -> bool {
    pattern::pattern_has_glob_metacharacters(text, true)
        || has_initial_closing_bracket_pattern(text)
}

const fn compound_kind(command: &ast::CompoundCommand) -> CompoundKind {
    match command {
        ast::CompoundCommand::Arithmetic(_) => CompoundKind::Arithmetic,
        ast::CompoundCommand::ArithmeticForClause(_) => CompoundKind::ArithmeticForClause,
        ast::CompoundCommand::BraceGroup(_) => CompoundKind::BraceGroup,
        ast::CompoundCommand::Subshell(_) => CompoundKind::Subshell,
        ast::CompoundCommand::ForClause(_) => CompoundKind::ForClause,
        ast::CompoundCommand::CaseClause(_) => CompoundKind::CaseClause,
        ast::CompoundCommand::IfClause(_) => CompoundKind::IfClause,
        ast::CompoundCommand::WhileClause(_) => CompoundKind::WhileClause,
        ast::CompoundCommand::UntilClause(_) => CompoundKind::UntilClause,
        ast::CompoundCommand::Coprocess(_) => CompoundKind::Coprocess,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    enum WordExpectation<'a> {
        Literal(&'a str),
        LocalizedText,
        Parameter,
        ParameterWithLiteral(&'a str),
        CommandSubstitution,
        Arithmetic,
        ArithmeticCommandSubstitution,
        TildeHome,
        TildeOther,
        AnsiCEscape,
        BraceExpansion,
        PathnamePattern,
    }

    fn project_test_word(raw: &str) -> Result<(ShellWord, ExecutionFeatures), ShellAnalysisError> {
        let mut analyzer = Analyzer {
            options: ParserOptions::default(),
            budget: AnalysisBudget::default(),
            nesting: 0,
            result: ShellProgram {
                commands: Vec::new(),
                compound_redirects: Vec::new(),
                features: ExecutionFeatures::default(),
            },
        };
        let word = analyzer.project_raw_word(raw, false)?;
        Ok((word, analyzer.result.features))
    }

    fn literal(word: Option<&ShellWord>) -> Option<&str> {
        word.and_then(|word| word.literal.as_deref())
    }

    fn literals(words: &[ShellWord]) -> Vec<&str> {
        words
            .iter()
            .map(|word| word.literal.as_deref().expect("structural test word"))
            .collect()
    }

    #[test]
    fn words_classify_literal_and_expansion_provenance() {
        for (input, expected) in [
            ("plain", WordExpectation::Literal("plain")),
            ("'single quoted'", WordExpectation::Literal("single quoted")),
            (
                "\"double quoted\"",
                WordExpectation::Literal("double quoted"),
            ),
            ("$\"gettext quoted\"", WordExpectation::LocalizedText),
            ("\\[abc\\]", WordExpectation::Literal("[abc]")),
            ("foo\\*", WordExpectation::Literal("foo*")),
            ("''\\[abc\\]", WordExpectation::Literal("[abc]")),
            ("\"\"foo\\*", WordExpectation::Literal("foo*")),
            ("'[abc]'", WordExpectation::Literal("[abc]")),
            ("\"[abc]\"", WordExpectation::Literal("[abc]")),
            ("\"*.tmp\"", WordExpectation::Literal("*.tmp")),
            ("\"/bin/r[]m]\"", WordExpectation::Literal("/bin/r[]m]")),
            ("'[abc]'\"*.tmp\"", WordExpectation::Literal("[abc]*.tmp")),
            ("foo[]", WordExpectation::Literal("foo[]")),
            ("foo[bar", WordExpectation::Literal("foo[bar")),
            ("foo{bar}", WordExpectation::Literal("foo{bar}")),
            ("$'rm'", WordExpectation::Literal("rm")),
            ("$target", WordExpectation::Parameter),
            (
                "$target$'suffix'",
                WordExpectation::ParameterWithLiteral("suffix"),
            ),
            ("$(resolve-target)", WordExpectation::CommandSubstitution),
            ("`resolve-target`", WordExpectation::CommandSubstitution),
            ("$((1 + 2))", WordExpectation::Arithmetic),
            (
                "$((1 + $(resolve-target)))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x:?$(resolve-target)} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x:$(resolve-offset)} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x:0:$(resolve-length)} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x/foo/$(resolve-replacement)} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x/$(resolve-pattern)/replacement} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x%$(resolve-suffix)} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${x^$(resolve-case-pattern)} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            (
                "$(( ${array[$(resolve-index)]} ))",
                WordExpectation::ArithmeticCommandSubstitution,
            ),
            ("~", WordExpectation::TildeHome),
            ("~+", WordExpectation::TildeOther),
            ("$'\\x72m'", WordExpectation::AnsiCEscape),
            ("/{,}", WordExpectation::BraceExpansion),
            ("{1..2}", WordExpectation::BraceExpansion),
            ("{a..b}", WordExpectation::BraceExpansion),
            ("/bin/r[m]", WordExpectation::PathnamePattern),
            ("/bin/r[]m]", WordExpectation::PathnamePattern),
            ("/bin/r[\\m]", WordExpectation::PathnamePattern),
            ("''/bin/r[\\m]", WordExpectation::PathnamePattern),
            ("\"\"/bin/r[\\m]", WordExpectation::PathnamePattern),
            ("*.tmp", WordExpectation::PathnamePattern),
            ("@(one|two)", WordExpectation::PathnamePattern),
        ] {
            let (word, features) = project_test_word(input).unwrap_or_else(|error| {
                panic!("{input}: expected {expected:?}, got analysis error {error:?}")
            });

            match expected {
                WordExpectation::Literal(expected) => {
                    assert_eq!(word.literal.as_deref(), Some(expected), "{input}");
                    assert!(
                        word.parts.iter().all(|part| matches!(
                            part,
                            WordPart::Literal(_) | WordPart::UnquotedLiteral(_)
                        )),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::LocalizedText => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        matches!(word.parts.as_slice(), [WordPart::LocalizedText]),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::Parameter => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::Parameter { .. })),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::ParameterWithLiteral(expected) => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::Parameter { .. })),
                        "{input}: {:?}",
                        word.parts
                    );
                    assert!(
                        word.parts.iter().any(
                            |part| matches!(part, WordPart::Literal(text) if text == expected)
                        ),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::CommandSubstitution => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::CommandSubstitution)),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::Arithmetic => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::Arithmetic)),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::ArithmeticCommandSubstitution => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::Arithmetic)),
                        "{input}: {:?}",
                        word.parts
                    );
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::CommandSubstitution)),
                        "{input}: {:?}",
                        word.parts
                    );
                    assert!(features.command_substitution, "{input}");
                }
                WordExpectation::TildeHome => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::TildeHome)),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::TildeOther => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        matches!(word.parts.as_slice(), [WordPart::TildeOther]),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::AnsiCEscape => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::AnsiCEscape)),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::BraceExpansion => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::BraceExpansion)),
                        "{input}: {:?}",
                        word.parts
                    );
                }
                WordExpectation::PathnamePattern => {
                    assert_eq!(word.literal, None, "{input}");
                    assert!(
                        word.parts
                            .iter()
                            .any(|part| matches!(part, WordPart::PathnamePattern)),
                        "{input}: {:?}",
                        word.parts
                    );
                }
            }
        }
    }

    #[test]
    fn arithmetic_word_marks_potential_shell_state_mutation() {
        let (arithmetic, _) = project_test_word("$((X=1))").unwrap();
        assert!(arithmetic.may_mutate_shell_state);

        let (nested, _) = project_test_word("${VALUE:-$((X=1))}").unwrap();
        assert!(nested.may_mutate_shell_state);

        let (literal, _) = project_test_word("literal").unwrap();
        assert!(!literal.may_mutate_shell_state);
    }

    #[test]
    fn parameter_arithmetic_operands_mark_potential_shell_state_mutation() {
        for input in [
            "${ARRAY[X=1]}",
            "${VALUE:X=1}",
            "${VALUE:0:X=1}",
            "\"${OUTER:-${VALUE:X=1}}\"",
            "\"${OUTER:-${ARRAY[X=1]}}\"",
        ] {
            let (word, _) = project_test_word(input).unwrap_or_else(|error| {
                panic!("{input}: {error:?}");
            });
            assert!(word.may_mutate_shell_state, "{input}: {:?}", word.parts);
        }

        for input in ["${VALUE}", "\"${OUTER:-literal}\""] {
            let (word, _) = project_test_word(input).unwrap_or_else(|error| {
                panic!("{input}: {error:?}");
            });
            assert!(!word.may_mutate_shell_state, "{input}: {:?}", word.parts);
        }
    }

    #[test]
    fn words_reject_failed_classification() {
        assert_eq!(
            project_test_word("\"unterminated").expect_err("unterminated word"),
            ShellAnalysisError::UnsupportedSyntax
        );
    }

    #[test]
    fn words_classify_each_maximal_unquoted_run() {
        for input in ["''/bin/r[\\m]", "\"\"/bin/r[\\m]"] {
            let (word, _) = project_test_word(input).expect(input);
            assert_eq!(word.literal, None, "{input}");
            assert!(
                word.parts
                    .iter()
                    .any(|part| matches!(part, WordPart::PathnamePattern)),
                "{input}: {:?}",
                word.parts
            );
        }

        for (input, expected) in [
            ("''\\[abc\\]", "[abc]"),
            ("\"\"foo\\*", "foo*"),
            ("'[abc]'\"*.tmp\"", "[abc]*.tmp"),
        ] {
            let (word, _) = project_test_word(input).expect(input);
            assert_eq!(word.literal.as_deref(), Some(expected), "{input}");
            assert!(
                word.parts.iter().all(|part| matches!(
                    part,
                    WordPart::Literal(_) | WordPart::UnquotedLiteral(_)
                )),
                "{input}: {:?}",
                word.parts
            );
        }
    }

    #[test]
    fn words_retain_adjacent_active_patterns_after_unquoted_parameters() {
        let (active, _) = project_test_word("${PREFIX}?").expect("active adjacent pattern");
        assert!(
            active
                .parts
                .iter()
                .any(|part| matches!(part, WordPart::UnquotedLiteral(text) if text == "?")),
            "{:?}",
            active.parts
        );
        assert!(
            active
                .parts
                .iter()
                .any(|part| matches!(part, WordPart::PathnamePattern)),
            "{:?}",
            active.parts
        );

        for input in ["${PREFIX}\"?\"", "${PREFIX}\\?", "${PREFIX}x"] {
            let (word, _) = project_test_word(input).expect(input);
            assert!(
                !word
                    .parts
                    .iter()
                    .any(|part| matches!(part, WordPart::PathnamePattern)),
                "{input}: {:?}",
                word.parts
            );
        }
    }

    #[test]
    fn words_keep_gettext_nonliteral() {
        let (word, _) = project_test_word("$\"localized\"").expect("gettext word");

        assert_eq!(word.literal, None);
        assert!(matches!(word.parts.as_slice(), [WordPart::LocalizedText]));
    }

    #[test]
    fn words_preserve_nested_assign_default_invalidations() {
        for input in [
            "\"${Y%${X:=-rf}}\"",
            "\"${Y/${X:=-rf}/b}\"",
            "\"${Y^${X:=-rf}}\"",
            "\"${Y:${X:=1}}\"",
            "\"${Y[${X:=1}]}\"",
            "\"$(( ${X:=1} ))\"",
            "$\"${Y%${X:=-rf}}\"",
            "\"${Y:-${X:=-rf}}\"",
            "\"${X:=a}${X:=b}\"",
        ] {
            let (word, _) = project_test_word(input).expect(input);
            assert_eq!(
                word.assign_default_invalidations,
                ["X"],
                "{input}: {:?}",
                word.parts
            );
        }

        let (ansi, _) = project_test_word("$'${X:=-rf}'").expect("ANSI-C word");
        assert!(
            ansi.assign_default_invalidations.is_empty(),
            "{:?}",
            ansi.parts
        );
    }

    #[test]
    fn words_find_command_substitution_in_arithmetic_parameter_operands() {
        for input in [
            "$(( ${x:-$(resolve-default)} ))",
            "$(( ${x:=$(resolve-assignment)} ))",
            "$(( ${x:?$(resolve-target)} ))",
            "$(( ${x:+$(resolve-alternative)} ))",
            "$(( ${x%$(resolve-small-suffix)} ))",
            "$(( ${x%%$(resolve-large-suffix)} ))",
            "$(( ${x#$(resolve-small-prefix)} ))",
            "$(( ${x##$(resolve-large-prefix)} ))",
            "$(( ${x:$(resolve-offset)} ))",
            "$(( ${x:0:$(resolve-length)} ))",
            "$(( ${x/foo/$(resolve-replacement)} ))",
            "$(( ${x/$(resolve-pattern)/replacement} ))",
            "$(( ${x^$(resolve-upper-first)} ))",
            "$(( ${x^^$(resolve-upper-all)} ))",
            "$(( ${x,$(resolve-lower-first)} ))",
            "$(( ${x,,$(resolve-lower-all)} ))",
            "$(( ${array[$(resolve-index)]} ))",
        ] {
            let (word, features) = project_test_word(input).expect(input);
            assert!(
                word.parts
                    .iter()
                    .any(|part| matches!(part, WordPart::Arithmetic)),
                "{input}: {:?}",
                word.parts
            );
            assert!(
                word.parts
                    .iter()
                    .any(|part| matches!(part, WordPart::CommandSubstitution)),
                "{input}: {:?}",
                word.parts
            );
            assert!(features.command_substitution, "{input}");
        }
    }

    #[test]
    fn words_arithmetic_command_substitution_preserves_all_signals() {
        let input = "$((1 + $(resolve-target)))";
        let (word, features) = project_test_word(input).expect(input);

        assert_eq!(word.literal, None);
        assert!(
            word.parts
                .iter()
                .any(|part| matches!(part, WordPart::Arithmetic))
        );
        assert!(
            word.parts
                .iter()
                .any(|part| matches!(part, WordPart::CommandSubstitution))
        );
        assert!(features.command_substitution);
    }

    #[test]
    fn structure_preserves_command_argument_and_redirect_positions() {
        for (input, command_index, command, arguments, redirect_count) in [
            ("rm>/dev/null -rf /", 0, Some("rm"), vec!["-rf", "/"], 1),
            ("echo ready | rm -rf /", 1, Some("rm"), vec!["-rf", "/"], 0),
            (
                "\"2\">/dev/null rm -rf /",
                0,
                Some("2"),
                vec!["rm", "-rf", "/"],
                1,
            ),
            ("((0 || rm -rf / 1))", 0, None, vec![], 0),
            ("[;]", 0, Some("["), vec![], 0),
        ] {
            let program = analyze(input).expect(input);
            let shell_command = program.commands.get(command_index).expect(input);

            assert_eq!(literal(shell_command.command.as_ref()), command, "{input}");
            assert_eq!(literals(&shell_command.arguments), arguments, "{input}");
            assert_eq!(shell_command.redirects.len(), redirect_count, "{input}");
        }
    }

    #[test]
    fn structure_keeps_attached_and_separated_redirects_out_of_arguments() {
        let attached = analyze("rm>/dev/null -rf /").expect("attached redirect");
        let separated = analyze("rm >/dev/null -rf /").expect("separated redirect");

        for program in [&attached, &separated] {
            let command = &program.commands[0];
            assert_eq!(literal(command.command.as_ref()), Some("rm"));
            assert_eq!(literals(&command.arguments), ["-rf", "/"]);
            assert_eq!(command.redirects.len(), 1);
        }
    }

    #[test]
    fn structure_preserves_assignments_before_the_command() {
        let program = analyze("MODE=fast rm -rf /").expect("assignment command");
        let command = &program.commands[0];

        assert_eq!(command.assignments.len(), 1);
        assert_eq!(command.assignments[0].name, "MODE");
        assert_eq!(
            command.assignments[0].value.literal.as_deref(),
            Some("fast")
        );
        assert_eq!(literal(command.command.as_ref()), Some("rm"));
        assert_eq!(literals(&command.arguments), ["-rf", "/"]);
    }

    #[test]
    fn structure_preserves_scalar_append_assignment_semantics() {
        let program = analyze("X=-; X+=rf").expect("scalar assignments");

        assert!(!program.commands[0].assignments[0].append);
        assert!(program.commands[1].assignments[0].append);
        assert_eq!(
            program.commands[1].assignments[0].value.literal.as_deref(),
            Some("rf")
        );
    }

    #[test]
    fn structure_preserves_suffix_assignment_words_in_argument_order() {
        let program = analyze("exec -a NAME=foo rm -rf /").expect("suffix assignment word");
        let command = &program.commands[0];

        assert!(command.assignments.is_empty());
        assert_eq!(literal(command.command.as_ref()), Some("exec"));
        assert_eq!(
            literals(&command.arguments),
            ["-a", "NAME=foo", "rm", "-rf", "/"]
        );
    }

    #[test]
    fn structure_visits_supported_control_flow_bodies() {
        for input in [
            "if true; then rm -rf /; else echo safe; fi",
            "while false; do rm -rf /; done",
            "until true; do rm -rf /; done",
            "for item in one; do rm -rf /; done",
            "for ((i=0; i<1; i++)); do rm -rf /; done",
        ] {
            let program = analyze(input).expect(input);
            assert!(
                program
                    .commands
                    .iter()
                    .any(|command| literal(command.command.as_ref()) == Some("rm")),
                "{input}"
            );
        }
    }

    #[test]
    fn structure_distinguishes_conditional_loop_pipeline_and_subshell_contexts() {
        for (input, command_name, expected) in [
            (
                "if true; then conditional; fi",
                "conditional",
                ExecutionContext::Conditional,
            ),
            (
                "while false; do repeated; done",
                "repeated",
                ExecutionContext::Loop,
            ),
            (
                "assigned=/tmp | consume",
                "consume",
                ExecutionContext::Pipeline,
            ),
            (
                "(assigned=/tmp)",
                "assigned=/tmp",
                ExecutionContext::Subshell,
            ),
        ] {
            let program = analyze(input).expect(input);
            let command = program
                .commands
                .iter()
                .find(|command| {
                    literal(command.command.as_ref()) == Some(command_name)
                        || command.assignments.iter().any(|assignment| {
                            format!(
                                "{}={}",
                                assignment.name,
                                assignment.value.literal.as_deref().unwrap_or_default()
                            ) == command_name
                        })
                })
                .expect(input);
            assert_eq!(command.context, expected, "{input}");
        }
    }

    #[test]
    fn structure_preserves_conditional_async_and_nested_restrictive_contexts() {
        let program = analyze(
            "first && second || third; background & \
             if true; then nested && later; fi | sink; \
             if condition; then inner_left | inner_right; fi",
        )
        .expect("program");
        for (name, expected) in [
            ("first", ExecutionContext::TopLevel),
            ("second", ExecutionContext::Conditional),
            ("third", ExecutionContext::Conditional),
            ("background", ExecutionContext::Asynchronous),
            ("true", ExecutionContext::Pipeline),
            ("nested", ExecutionContext::Pipeline),
            ("later", ExecutionContext::Pipeline),
            ("sink", ExecutionContext::Pipeline),
            ("condition", ExecutionContext::Conditional),
            ("inner_left", ExecutionContext::Pipeline),
            ("inner_right", ExecutionContext::Pipeline),
        ] {
            let command = program
                .commands
                .iter()
                .find(|command| literal(command.command.as_ref()) == Some(name))
                .unwrap_or_else(|| panic!("{name}"));
            assert_eq!(command.context, expected, "{name}");
        }
    }

    #[test]
    fn structure_marks_execution_bearing_syntax() {
        let process = analyze("cat <(rm -rf /)").expect("process substitution");
        assert!(process.features.process_substitution);
        assert!(
            process
                .commands
                .iter()
                .any(|command| { literal(command.command.as_ref()) == Some("rm") })
        );

        for input in ["{ rm -rf /; }", "(rm -rf /)", "coproc rm -rf /"] {
            let program = analyze(input).expect(input);
            assert!(program.features.executable_group, "{input}");
            assert!(
                program
                    .commands
                    .iter()
                    .any(|command| { literal(command.command.as_ref()) == Some("rm") })
            );
        }
    }

    #[test]
    fn structure_preserves_process_substitution_position_and_context() {
        let program =
            analyze("cat before <(rm -rf /) after").expect("positional process substitution");
        let cat = program
            .commands
            .iter()
            .find(|command| literal(command.command.as_ref()) == Some("cat"))
            .expect("cat command");
        let rm = program
            .commands
            .iter()
            .find(|command| literal(command.command.as_ref()) == Some("rm"))
            .expect("nested rm command");

        assert_eq!(cat.context, ExecutionContext::TopLevel);
        assert_eq!(literals(&cat.arguments[..1]), ["before"]);
        assert!(matches!(
            cat.arguments[1].parts.as_slice(),
            [WordPart::ProcessSubstitution]
        ));
        assert_eq!(cat.arguments[1].raw, "<(...)");
        assert_eq!(cat.arguments[1].literal, None);
        assert_eq!(literals(&cat.arguments[2..]), ["after"]);
        assert_eq!(rm.context, ExecutionContext::ProcessSubstitution);
    }

    #[test]
    fn structure_rejects_deep_process_substitution_at_semantic_depth_bound() {
        let mut input = "echo safe".to_owned();
        for _ in 0..80 {
            input = format!("cat <({input})");
        }

        let result = std::panic::catch_unwind(|| analyze(&input));
        assert_eq!(
            result
                .expect("deep process substitution must not panic")
                .expect_err("deep process substitution"),
            ShellAnalysisError::ResourceLimit
        );
    }

    #[test]
    fn structure_distinguishes_pipeline_group_and_subshell_contexts() {
        for (input, command_name, expected) in [
            (
                "echo ready | consume",
                "consume",
                ExecutionContext::Pipeline,
            ),
            ("{ grouped; }", "grouped", ExecutionContext::Group),
            ("(subshell)", "subshell", ExecutionContext::Subshell),
        ] {
            let program = analyze(input).expect(input);
            let command = program
                .commands
                .iter()
                .find(|command| literal(command.command.as_ref()) == Some(command_name))
                .expect(input);
            assert_eq!(command.context, expected, "{input}");
        }
    }

    #[test]
    fn structure_marks_command_substitution() {
        let program = analyze("echo $(rm -rf /)").expect("command substitution");

        assert!(program.features.command_substitution);
    }

    #[test]
    fn structure_keeps_extended_tests_inert_and_redirects_structural() {
        let program = analyze("[[ -n value ]] >/dev/null").expect("extended test");
        let command = &program.commands[0];

        assert_eq!(literal(command.command.as_ref()), None);
        assert!(command.arguments.is_empty());
        assert_eq!(command.redirects.len(), 1);
    }

    #[test]
    fn structure_visits_extended_test_words_for_execution_features() {
        let program =
            analyze(r#"[[ -n "$(resolve-value)" ]]"#).expect("extended test substitution");
        let command = &program.commands[0];

        assert_eq!(literal(command.command.as_ref()), None);
        assert!(command.arguments.is_empty());
        assert!(program.features.command_substitution);
    }

    #[test]
    fn structure_retains_redirects_on_every_compound_form() {
        for (input, expected_kind) in [
            ("{ echo brace; } >/dev/null", CompoundKind::BraceGroup),
            ("(echo subshell) 2>/dev/null", CompoundKind::Subshell),
            (
                "if true; then echo control; fi </dev/null",
                CompoundKind::IfClause,
            ),
        ] {
            let program = analyze(input).expect(input);
            assert_eq!(program.compound_redirects.len(), 1, "{input}");
            let record = &program.compound_redirects[0];
            assert_eq!(record.context, ExecutionContext::TopLevel, "{input}");
            assert_eq!(record.kind, expected_kind, "{input}");
            assert_eq!(record.redirects.len(), 1, "{input}");
            assert_eq!(
                record.redirects[0]
                    .target
                    .as_ref()
                    .and_then(|word| word.literal.as_deref()),
                Some("/dev/null"),
                "{input}"
            );
        }
    }

    #[test]
    fn structure_rejects_unsupported_function_and_case_forms() {
        for input in ["danger() { rm -rf /; }", "case x in x) rm -rf /;; esac"] {
            assert_eq!(
                analyze(input).expect_err(input),
                ShellAnalysisError::UnsupportedSyntax,
                "{input}"
            );
        }
    }

    #[test]
    fn structure_enforces_the_analysis_budget() {
        let input = std::iter::repeat_n("x", 70_000)
            .collect::<Vec<_>>()
            .join(" ");

        assert_eq!(
            analyze(&input).expect_err("oversized AST"),
            ShellAnalysisError::ResourceLimit
        );
    }

    #[test]
    fn structure_analysis_budget_allows_exact_limit_and_rejects_next_visit() {
        let mut budget = AnalysisBudget::default();

        for visit in 1..=MAX_ANALYSIS_NODES {
            assert_eq!(budget.visit(), Ok(()), "visit {visit}");
        }
        assert_eq!(budget.visited, MAX_ANALYSIS_NODES);
        assert_eq!(budget.visit(), Err(ShellAnalysisError::ResourceLimit));
    }

    #[test]
    fn structure_rejects_deep_nesting_before_stack_exhaustion() {
        let mut input = "echo safe".to_owned();
        for _ in 0..80 {
            input = format!("{{ {input}; }}");
        }

        assert_eq!(
            analyze(&input).expect_err("deeply nested groups"),
            ShellAnalysisError::ResourceLimit
        );
    }

    #[test]
    fn structure_classifies_unquoted_text_with_word_helpers() {
        for (input, word_index, expected_raw, expected_literal) in [
            ("/bin/r[m] -rf /", 0, "/bin/r[m]", None),
            ("echo foo{bar}", 1, "foo{bar}", Some("foo{bar}")),
        ] {
            let program = analyze(input).expect(input);
            let command = &program.commands[0];
            let word = if word_index == 0 {
                command.command.as_ref().expect(input)
            } else {
                &command.arguments[word_index - 1]
            };

            assert_eq!(word.raw, expected_raw, "{input}");
            assert_eq!(word.literal.as_deref(), expected_literal, "{input}");
        }

        let quoted = analyze("echo 'foo{bar}' '/bin/r[m]'").expect("quoted text");
        assert_eq!(
            literals(&quoted.commands[0].arguments),
            ["foo{bar}", "/bin/r[m]"]
        );
    }
}
