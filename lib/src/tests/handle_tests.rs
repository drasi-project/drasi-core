use super::super::*;

#[tokio::test]
async fn test_source_handle_for_nonexistent_source() {
    let core = DrasiLib::builder().build().await.unwrap();

    let result = core.source_handle("nonexistent").await;
    assert!(
        result.is_err(),
        "Should return error for nonexistent source"
    );

    match result {
        Err(DrasiError::ComponentNotFound { kind, id }) => {
            assert_eq!(kind, "source");
            assert_eq!(id, "nonexistent");
        }
        _ => panic!("Expected ComponentNotFound error"),
    }
}

#[tokio::test]
async fn test_reaction_handle_for_nonexistent_reaction() {
    let core = DrasiLib::builder().build().await.unwrap();

    let result = core.reaction_handle("nonexistent").await;
    assert!(
        result.is_err(),
        "Should return error for nonexistent reaction"
    );

    match result {
        Err(DrasiError::ComponentNotFound { kind, id }) => {
            assert_eq!(kind, "reaction");
            assert_eq!(id, "nonexistent");
        }
        _ => panic!("Expected ComponentNotFound error"),
    }
}
