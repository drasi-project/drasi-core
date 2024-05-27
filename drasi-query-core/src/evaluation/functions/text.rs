#[cfg(test)]
mod tests;
mod text;

use std::sync::Arc;

use self::text::{
    LTrim, Left, RTrim, Replace, Reverse, Right, Split, Substring, ToLower, ToString,
    ToStringOrNull, ToUpper, Trim,
};

use super::{Function, FunctionRegistry};

pub trait RegisterTextFunctions {
    fn register_text_functions(&self);
}

impl RegisterTextFunctions for FunctionRegistry {
    fn register_text_functions(&self) {
        self.register_function("toUpper", Function::Scalar(Arc::new(ToUpper {})));
        self.register_function("toLower", Function::Scalar(Arc::new(ToLower {})));
        self.register_function("trim", Function::Scalar(Arc::new(Trim {})));
        self.register_function("ltrim", Function::Scalar(Arc::new(LTrim {})));
        self.register_function("rtrim", Function::Scalar(Arc::new(RTrim {})));
        self.register_function("reverse", Function::Scalar(Arc::new(Reverse {})));
        self.register_function("left", Function::Scalar(Arc::new(Left {})));
        self.register_function("right", Function::Scalar(Arc::new(Right {})));
        self.register_function("replace", Function::Scalar(Arc::new(Replace {})));
        self.register_function("split", Function::Scalar(Arc::new(Split {})));
        self.register_function("substring", Function::Scalar(Arc::new(Substring {})));
        self.register_function("toString", Function::Scalar(Arc::new(ToString {})));
        self.register_function(
            "toStringOrNull",
            Function::Scalar(Arc::new(ToStringOrNull {})),
        );
    }
}
