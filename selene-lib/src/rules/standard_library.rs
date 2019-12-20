use super::{super::standard_library::*, *};
use crate::ast_util::scopes::ScopeManager;
use std::convert::Infallible;

use full_moon::{
    ast::{self, Ast},
    node::Node,
    tokenizer::{Position, Symbol, TokenType},
    visitors::Visitor,
};

pub struct StandardLibraryLint;

impl Rule for StandardLibraryLint {
    type Config = ();
    type Error = Infallible;

    fn new(_: Self::Config) -> Result<Self, Self::Error> {
        Ok(StandardLibraryLint)
    }

    fn pass(&self, ast: &Ast, context: &Context) -> Vec<Diagnostic> {
        let mut visitor = StandardLibraryVisitor {
            diagnostics: Vec::new(),
            scope_manager: ScopeManager::new(ast),
            standard_library: &context.standard_library,
        };

        visitor.visit_ast(ast);

        visitor.diagnostics
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn rule_type(&self) -> RuleType {
        RuleType::Correctness
    }
}

fn take_while_keep_going(suffix: &ast::Suffix, keep_going: &mut bool) -> bool {
    let result = *keep_going;
    *keep_going = if let ast::Suffix::Call(_) = suffix {
        false
    } else {
        true
    };
    result
}

fn name_path_from_prefix_suffix<'a, 'ast, S: Iterator<Item = &'a ast::Suffix<'ast>>>(
    prefix: &'a ast::Prefix<'ast>,
    suffixes: S,
) -> Option<Vec<String>> {
    if let ast::Prefix::Name(ref name) = prefix {
        let mut names = Vec::new();
        names.push(name.to_string());

        let mut keep_going = true;

        for suffix in suffixes.take_while(|suffix| take_while_keep_going(suffix, &mut keep_going)) {
            match suffix {
                ast::Suffix::Call(call) => {
                    if let ast::Call::MethodCall(method_call) = call {
                        names.push(method_call.name().to_string());
                    }
                }

                ast::Suffix::Index(index) => {
                    if let ast::Index::Dot { name, .. } = index {
                        names.push(name.to_string());
                    } else {
                        return None;
                    }
                }
            }
        }

        Some(names)
    } else {
        None
    }
}

fn name_path<'a, 'ast>(expression: &'a ast::Expression<'ast>) -> Option<Vec<String>> {
    if let ast::Expression::Value { value, .. } = expression {
        if let ast::Value::Var(var) = &**value {
            match var {
                ast::Var::Expression(expression) => {
                    name_path_from_prefix_suffix(expression.prefix(), expression.iter_suffixes())
                }

                ast::Var::Name(name) => Some(vec![name.to_string()]),
            }
        } else {
            None
        }
    } else {
        None
    }
}

// Returns the argument type of the expression if it can be constantly resolved
// Otherwise, returns None
// Only attempts to resolve constants
fn get_argument_type(expression: &ast::Expression) -> Option<PassedArgumentType> {
    match expression {
        ast::Expression::Parentheses { expression, .. } => get_argument_type(expression),

        ast::Expression::UnaryOperator { unop, expression } => {
            match unop {
                // CAVEAT: If you're overriding __len on a userdata and then making it not return a number
                // ...sorry, but I don't care about your code :)
                ast::UnOp::Hash(_) => Some(ArgumentType::Number.into()),
                ast::UnOp::Minus(_) => get_argument_type(expression),
                ast::UnOp::Not(_) => Some(ArgumentType::Bool.into()),
            }
        }

        ast::Expression::Value { binop: rhs, value } => {
            let base = match &**value {
                ast::Value::Function(_) => Some(ArgumentType::Function.into()),
                ast::Value::FunctionCall(_) => None,
                ast::Value::Number(_) => Some(ArgumentType::Number.into()),
                ast::Value::ParseExpression(expression) => get_argument_type(expression),
                ast::Value::String(token) => {
                    Some(PassedArgumentType::from_string(token.to_string()))
                }
                ast::Value::Symbol(symbol) => match *symbol.token_type() {
                    TokenType::Symbol { symbol } => match symbol {
                        Symbol::False => Some(ArgumentType::Bool.into()),
                        Symbol::True => Some(ArgumentType::Bool.into()),
                        Symbol::Nil => Some(ArgumentType::Nil.into()),
                        Symbol::Ellipse => Some(ArgumentType::Vararg.into()),
                        ref other => {
                            unreachable!("TokenType::Symbol was not expected ({:?})", other)
                        }
                    },

                    ref other => unreachable!(
                        "ast::Value::Symbol token_type != TokenType::Symbol ({:?})",
                        other
                    ),
                },
                ast::Value::TableConstructor(_) => Some(ArgumentType::Table.into()),
                ast::Value::Var(_) => None,
            };

            if let Some(rhs) = dbg!(rhs) {
                // Nearly all of these will return wrong results if you have a non-idiomatic metatable
                // I intentionally omitted common metamethod re-typings, like __mul
                match rhs.bin_op() {
                    ast::BinOp::Caret(_) => Some(ArgumentType::Number.into()),

                    ast::BinOp::GreaterThan(_)
                    | ast::BinOp::GreaterThanEqual(_)
                    | ast::BinOp::LessThan(_)
                    | ast::BinOp::LessThanEqual(_)
                    | ast::BinOp::TwoEqual(_)
                    | ast::BinOp::TildeEqual(_) => {
                        if_chain::if_chain! {
                            if let ast::Expression::Value { binop: rhs, .. } = rhs.rhs();
                            if let Some(rhs) = rhs;
                            if let ast::BinOp::And(_) | ast::BinOp::Or(_) = rhs.bin_op();
                            then {
                                None
                            } else {
                                Some(ArgumentType::Bool.into())
                            }
                        }
                    }

                    // Basic types will often re-implement these (e.g. Roblox's Vector3)
                    ast::BinOp::Plus(_)
                    | ast::BinOp::Minus(_)
                    | ast::BinOp::Star(_)
                    | ast::BinOp::Slash(_) => base,

                    ast::BinOp::Percent(_) => Some(ArgumentType::Number.into()),

                    ast::BinOp::TwoDots(_) => Some(ArgumentType::String.into()),

                    ast::BinOp::And(_) | ast::BinOp::Or(_) => {
                        // We could potentially support union types here
                        // Or even just produce one type if both the left and right sides can be evaluated
                        // But for now, the evaluation just isn't smart enough to where this would be practical
                        None
                    }
                }
            } else {
                base
            }
        }
    }
}

pub struct StandardLibraryVisitor<'std> {
    diagnostics: Vec<Diagnostic>,
    scope_manager: ScopeManager,
    standard_library: &'std StandardLibrary,
}

impl StandardLibraryVisitor<'_> {
    fn lint_invalid_field_access(
        &mut self,
        mut name_path: Vec<String>,
        range: (Position, Position),
    ) {
        if self.standard_library.find_global(&name_path).is_none()
            && self
                .standard_library
                .find_global(&[name_path[0].to_owned()])
                .is_some()
        // Make sure it's not just `bad()`
        {
            let field = name_path.pop().unwrap();
            assert!(!name_path.is_empty(), "name_path is empty");

            // check if it's writable
            for bound in 1..=name_path.len() {
                let path = &name_path[0..bound];
                match self.standard_library.find_global(path) {
                    Some(field) => {
                        match field {
                            Field::Any => return,

                            Field::Property { writable } => {
                                if writable.is_some() && *writable != Some(Writable::Overridden) {
                                    return;
                                }
                            }

                            _ => {}
                        };
                    }

                    None => break,
                }
            }

            self.diagnostics.push(Diagnostic::new_complete(
                "incorrect_standard_library_use",
                format!(
                    "standard library global `{}` does not contain the field `{}`",
                    name_path.join("."),
                    field,
                ),
                Label::new((range.0.bytes(), range.1.bytes())),
                Vec::new(),
                Vec::new(),
            ));
        }
    }
}

impl Visitor<'_> for StandardLibraryVisitor<'_> {
    fn visit_assignment(&mut self, assignment: &ast::Assignment) {
        for var in assignment.var_list() {
            if let Some(reference) = self
                .scope_manager
                .reference_at_byte(var.start_position().unwrap().bytes())
            {
                if reference.resolved.is_some() {
                    return;
                }
            }

            match var {
                ast::Var::Expression(var_expr) => {
                    let mut keep_going = true;
                    if var_expr
                        .iter_suffixes()
                        .take_while(|suffix| take_while_keep_going(suffix, &mut keep_going))
                        .count()
                        != var_expr.iter_suffixes().count()
                    {
                        // Modifying the return value, which we don't lint yet
                        continue;
                    }

                    if let Some(name_path) =
                        name_path_from_prefix_suffix(var_expr.prefix(), var_expr.iter_suffixes())
                    {
                        match self.standard_library.find_global(&name_path) {
                            Some(field) => {
                                match field {
                                    Field::Property { writable } => {
                                        if writable.is_some()
                                            && *writable != Some(Writable::NewFields)
                                        {
                                            continue;
                                        }
                                    }
                                    Field::Any => continue,
                                    _ => {}
                                };

                                let range = var_expr.range().unwrap();

                                self.diagnostics.push(Diagnostic::new_complete(
                                    "incorrect_standard_library_use",
                                    format!(
                                        "standard library global `{}` is not writable",
                                        name_path.join("."),
                                    ),
                                    Label::new((range.0.bytes(), range.1.bytes())),
                                    Vec::new(),
                                    Vec::new(),
                                ));
                            }

                            None => {
                                self.lint_invalid_field_access(
                                    name_path,
                                    var_expr.range().unwrap(),
                                );
                            }
                        }
                    }
                }

                ast::Var::Name(name_token) => {
                    let name = name_token.to_string();

                    if let Some(global) = self.standard_library.find_global(&[name.to_owned()]) {
                        match global {
                            Field::Property { writable } => {
                                if writable.is_some() && *writable != Some(Writable::NewFields) {
                                    continue;
                                }
                            }
                            Field::Any => continue,
                            _ => {}
                        };

                        let range = name_token.range().unwrap();

                        self.diagnostics.push(Diagnostic::new_complete(
                            "incorrect_standard_library_use",
                            format!("standard library global `{}` is not overridable", name,),
                            Label::new((range.0.bytes(), range.1.bytes())),
                            Vec::new(),
                            Vec::new(),
                        ));
                    }
                }
            }
        }
    }

    fn visit_expression(&mut self, expression: &ast::Expression) {
        if let Some(reference) = self
            .scope_manager
            .reference_at_byte(expression.start_position().unwrap().bytes())
        {
            if reference.resolved.is_some() {
                return;
            }
        }

        if let Some(name_path) = name_path(expression) {
            self.lint_invalid_field_access(name_path, expression.range().unwrap());
        }
    }

    fn visit_function_call(&mut self, call: &ast::FunctionCall) {
        if let Some(reference) = self
            .scope_manager
            .reference_at_byte(call.start_position().unwrap().bytes())
        {
            if reference.resolved.is_some() {
                return;
            }
        }

        let mut keep_going = true;
        let mut suffixes: Vec<&ast::Suffix> = call
            .iter_suffixes()
            .take_while(|suffix| take_while_keep_going(suffix, &mut keep_going))
            .collect();

        let mut name_path =
            match name_path_from_prefix_suffix(call.prefix(), suffixes.iter().copied()) {
                Some(name_path) => name_path,
                None => return,
            };

        let call_suffix = suffixes.pop().unwrap();

        let field = match self.standard_library.find_global(&name_path) {
            Some(field) => field,
            None => {
                self.lint_invalid_field_access(
                    name_path,
                    (
                        call.prefix().start_position().unwrap(),
                        if let ast::Suffix::Call(ast::Call::MethodCall(method_call)) = call_suffix {
                            method_call.name().end_position().unwrap()
                        } else {
                            suffixes
                                .last()
                                .and_then(|suffix| suffix.end_position())
                                .unwrap_or_else(|| call.prefix().end_position().unwrap())
                        },
                    ),
                );
                return;
            }
        };

        let (arguments, expecting_method) = match &field {
            standard_library::Field::Any => return,
            standard_library::Field::Function { arguments, method } => (arguments, method),
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    "incorrect_standard_library_use",
                    format!(
                        "standard library field `{}` is not a function",
                        name_path.join("."),
                    ),
                    Label::from_node(call, None),
                ));

                return;
            }
        };

        let (function_args, call_is_method) = match call_suffix {
            ast::Suffix::Call(call) => match call {
                ast::Call::AnonymousCall(args) => (args, false),
                ast::Call::MethodCall(method_call) => (method_call.args(), true),
            },

            _ => unreachable!("function_call.call_suffix != ast::Suffix::Call"),
        };

        if *expecting_method != call_is_method {
            let problem = if call_is_method {
                "is not a method"
            } else {
                "is a method"
            };

            let using = if call_is_method { ":" } else { "." };
            let use_instead = if call_is_method { "." } else { ":" };

            let name = name_path.pop().unwrap();

            self.diagnostics.push(Diagnostic::new_complete(
                "incorrect_standard_library_use",
                format!(
                    "standard library function `{}{}{}` {}",
                    name_path.join("."),
                    using,
                    name,
                    problem,
                ),
                Label::from_node(call, None),
                vec![format!(
                    "try: {}{}{}(...)",
                    name_path.join("."),
                    use_instead,
                    name
                )],
                Vec::new(),
            ));

            return;
        }

        let mut argument_types = Vec::new();

        match function_args {
            ast::FunctionArgs::Parentheses { arguments, .. } => {
                for argument in arguments {
                    argument_types.push((argument.range().unwrap(), get_argument_type(argument)));
                }
            }

            ast::FunctionArgs::String(token) => {
                argument_types.push((
                    token.range().unwrap(),
                    Some(PassedArgumentType::from_string(token.to_string())),
                ));
            }

            ast::FunctionArgs::TableConstructor(table) => {
                argument_types.push((table.range().unwrap(), Some(ArgumentType::Table.into())));
            }
        }

        let mut expected_args = arguments
            .iter()
            .filter(|arg| arg.required != Required::NotRequired)
            .count();

        let mut vararg = false;
        let mut max_args = arguments.len();

        if let Some(last) = arguments.last() {
            if last.argument_type == ArgumentType::Vararg {
                if let Required::Required(message) = &last.required {
                    // Functions like math.ceil where not using the vararg is wrong
                    if arguments.len() > argument_types.len() {
                        self.diagnostics.push(Diagnostic::new_complete(
                            "incorrect_standard_library_use",
                            format!(
                                // TODO: This message isn't great
                                "standard library function `{}` requires use of the vararg",
                                name_path.join("."),
                            ),
                            Label::from_node(call, None),
                            message.iter().cloned().collect(),
                            Vec::new(),
                        ));
                    }

                    expected_args -= 1;
                    max_args -= 1;
                }

                vararg = true;
            }
        }

        let any_are_vararg = argument_types.iter().any(|(_, argument_type)| {
            argument_type.as_ref() == Some(&PassedArgumentType::Primitive(ArgumentType::Vararg))
        });

        if (!any_are_vararg && argument_types.len() < expected_args)
            || (!vararg && argument_types.len() > max_args)
        {
            self.diagnostics.push(Diagnostic::new(
                "incorrect_standard_library_use",
                format!(
                    "standard library function `{}` requires {} parameters, {} passed",
                    name_path.join("."),
                    expected_args,
                    argument_types.len(),
                ),
                Label::from_node(call, None),
            ));
        }

        for ((range, passed_type), expected) in argument_types.iter().zip(arguments.iter()) {
            if expected.argument_type == ArgumentType::Vararg {
                continue;
            }

            if let Some(passed_type) = passed_type {
                let matches = passed_type.matches(&expected.argument_type);

                if !matches {
                    self.diagnostics.push(Diagnostic::new(
                        "incorrect_standard_library_use",
                        format!(
                            "use of standard_library function `{}` is incorrect",
                            name_path.join("."),
                        ),
                        Label::new_with_message(
                            (range.0.bytes() as u32, range.1.bytes() as u32),
                            format!(
                                "expected `{}`, received `{}`",
                                expected.argument_type,
                                passed_type.type_name()
                            ),
                        ),
                    ));
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PassedArgumentType {
    Primitive(ArgumentType),
    String(String),
}

impl PassedArgumentType {
    fn from_string(mut string: String) -> PassedArgumentType {
        string.pop();
        PassedArgumentType::String(string.chars().skip(1).collect())
    }

    fn matches(&self, argument_type: &ArgumentType) -> bool {
        if argument_type == &ArgumentType::Any {
            return true;
        }

        match self {
            PassedArgumentType::Primitive(us) => {
                us == &ArgumentType::Vararg
                    || us == argument_type
                    || (us == &ArgumentType::String
                        && match argument_type {
                            ArgumentType::Constant(_) => true,
                            _ => false,
                        })
            }
            PassedArgumentType::String(text) => match argument_type {
                ArgumentType::Constant(constants) => constants.contains(text),
                ArgumentType::String => true,
                _ => false,
            },
        }
    }

    fn type_name(&self) -> String {
        match self {
            PassedArgumentType::Primitive(argument_type) => argument_type.to_string(),
            PassedArgumentType::String(_) => ArgumentType::String.to_string(),
        }
    }
}

impl From<ArgumentType> for PassedArgumentType {
    fn from(argument_type: ArgumentType) -> Self {
        PassedArgumentType::Primitive(argument_type)
    }
}

#[cfg(test)]
mod tests {
    use super::{super::test_util::*, *};

    #[test]
    fn test_name_path() {
        let ast = full_moon::parse("local x = foo; local y = foo.bar.baz").unwrap();

        struct NamePathTestVisitor {
            paths: Vec<Vec<String>>,
        }

        impl Visitor<'_> for NamePathTestVisitor {
            fn visit_local_assignment(&mut self, node: &ast::LocalAssignment) {
                self.paths.push(
                    name_path(node.expr_list().into_iter().next().unwrap())
                        .expect("name_path returned None"),
                );
            }
        }

        let mut visitor = NamePathTestVisitor { paths: Vec::new() };

        visitor.visit_ast(&ast);

        assert_eq!(
            visitor.paths,
            vec![
                vec!["foo".to_owned()],
                vec!["foo".to_owned(), "bar".to_owned(), "baz".to_owned()],
            ]
        );
    }

    #[test]
    fn test_any() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "any",
        );
    }

    #[test]
    fn test_bad_call_signatures() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "bad_call_signatures",
        );
    }

    #[test]
    fn test_complex() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "complex",
        );
    }

    #[test]
    fn test_constants() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "constants",
        );
    }

    #[test]
    fn test_lua52() {
        test_lint_config(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "lua52",
            TestUtilConfig {
                standard_library: StandardLibrary::from_name("lua52").unwrap(),
                ..TestUtilConfig::default()
            },
        );
    }

    #[test]
    fn test_method_call() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "method_call",
        );
    }

    #[test]
    fn test_shadowing() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "shadowing",
        );
    }

    #[test]
    fn test_ternary() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "ternary",
        );
    }

    #[test]
    fn test_unknown_property() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "unknown_property",
        );
    }

    #[test]
    fn test_vararg() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "vararg",
        );
    }

    #[test]
    fn test_wildcard() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "wildcard",
        );
    }

    #[test]
    fn test_wildcard_structs() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "wildcard_structs",
        );
    }

    #[test]
    fn test_writing() {
        test_lint(
            StandardLibraryLint::new(()).unwrap(),
            "standard_library",
            "writing",
        );
    }
}
