// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use drasi_query_ast::ast;

use super::text;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("sha256"),
        args: vec![],
        position_in_query: 10,
    }
}

async fn digest(input: &str) -> String {
    let sha256 = text::Sha256 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = sha256
        .call(
            &context,
            &get_func_expr(),
            vec![VariableValue::String(input.to_string())],
        )
        .await
        .unwrap();

    match result {
        VariableValue::String(hex) => hex,
        other => panic!("expected a string result, got {other:?}"),
    }
}

#[tokio::test]
async fn test_sha256_empty_string() {
    // `printf '' | shasum -a 256`
    assert_eq!(
        digest("").await,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[tokio::test]
async fn test_sha256_known_ascii_vector() {
    // NIST FIPS 180-2 vector for "abc".
    assert_eq!(
        digest("abc").await,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[tokio::test]
async fn test_sha256_hashes_exact_utf8_bytes_of_unicode() {
    // `printf 'héllo wörld 🌍' | shasum -a 256`: hashed as UTF-8 bytes, with no
    // Unicode normalization applied.
    assert_eq!(
        digest("héllo wörld 🌍").await,
        "701aea0197ece166311a45663e52d5d580e3b5ff116dfda2724ad928e51a834a"
    );
}

#[tokio::test]
async fn test_sha256_is_lowercase_hex_of_fixed_length() {
    let hex = digest("abc").await;
    assert_eq!(hex.len(), 64);
    assert!(hex
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
}

#[tokio::test]
async fn test_sha256_preserves_newlines_and_surrounding_whitespace() {
    // `printf 'line1\nline2' | shasum -a 256`
    assert_eq!(
        digest("line1\nline2").await,
        "683376e290829b482c2655745caffa7a1dccfa10afaa62dac2b42dd6c68d0f83"
    );
    // Removing the newline changes the digest, so newlines are not stripped.
    assert_eq!(
        digest("line1line2").await,
        "e92170357021c40ec6f32421648ab49114c85082e211ad5400d4a7d3c7a5360c"
    );
    // A trailing newline changes the digest, so nothing is trimmed.
    assert_eq!(
        digest("line1\nline2\n").await,
        "2751a3a2f303ad21752038085e2b8c5f98ecff61a2e4ebbd43506a941725be80"
    );
    assert_ne!(digest("line1\nline2").await, digest("line1line2").await);
    assert_ne!(digest("line1\nline2").await, digest("line1\nline2\n").await);
    assert_ne!(digest(" abc ").await, digest("abc").await);
}

#[tokio::test]
async fn test_sha256_matches_workgraph_run_preimage_vector() {
    // The locked WorkGraph run preimage: domain, project item node id, subject
    // node id and body digest joined by single LF bytes, with no trailing LF.
    let preimage = concat!(
        "workgraph.run/v1\n",
        "PVTI_lADOABCDEF4AbcDEzgXYZ123\n",
        "I_kwDOABCDEF6ABCDE\n",
        "sha256:09a16cabf7f29fd03469340079d25d1de2e818149c13f982d8133a87cbc8a5d1"
    );
    assert_eq!(
        digest(preimage).await,
        "775813253e0b6106e5a5f40ea02dcee45021121ce3f79f2d23c180d9b3027664"
    );
}

#[tokio::test]
async fn test_sha256_null() {
    let sha256 = text::Sha256 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = sha256
        .call(&context, &get_func_expr(), vec![VariableValue::Null])
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_sha256_invalid_args() {
    let sha256 = text::Sha256 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    for arg in [
        VariableValue::Integer(123.into()),
        VariableValue::Bool(true),
        VariableValue::List(vec![VariableValue::String("abc".to_string())]),
    ] {
        let result = sha256.call(&context, &get_func_expr(), vec![arg]).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgument(0)
            }
        ));
    }
}

#[tokio::test]
async fn test_sha256_too_few_args() {
    let sha256 = text::Sha256 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = sha256.call(&context, &get_func_expr(), vec![]).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_sha256_too_many_args() {
    let sha256 = text::Sha256 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("abc".to_string()),
        VariableValue::String("def".to_string()),
    ];
    let result = sha256.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}
