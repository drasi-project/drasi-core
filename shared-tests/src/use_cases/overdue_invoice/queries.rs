use drasi_query_ast::ast;

pub fn list_overdue_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
    MATCH 
        (a:Invoice)
    WHERE drasi.trueUntil(
        a.status = 'unpaid', 
        date(a.invoiceDate) + duration({days: 3})
    )
    RETURN
        a.invoiceNumber as invoiceNumber,
        a.invoiceDate as invoiceDate
    ",
    )
    .unwrap()
}

pub fn count_overdue_greater_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
    MATCH 
        (a:Invoice)
    WHERE a.status = 'unpaid'
    WITH
        count(a) as count
    WHERE drasi.trueUntil(count > 2, date.transaction() + duration({days: 3}))
    RETURN
        count
    ",
    )
    .unwrap()
}
