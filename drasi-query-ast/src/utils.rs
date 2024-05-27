use crate::ast::{self, ParentExpression};

const AGGRRGATING_FUNCS: [&str; 7] = [
    "count",
    "sum",
    "min",
    "max",
    "avg",
    "drasi.linearGradient",
    "drasi.last",
];

//TODO: provide a way extract a list of aggregating functions from function registry

pub fn contains_aggregating_function(expression: &ast::Expression) -> bool {
    let stack = &mut vec![expression];

    while let Some(expr) = stack.pop() {
        if let ast::Expression::FunctionExpression(ref function) = expr {
            if AGGRRGATING_FUNCS.contains(&function.name.as_ref()) {
                return true;
            }
        }

        for c in expr.get_children() {
            stack.push(c);
        }
    }

    false
}
