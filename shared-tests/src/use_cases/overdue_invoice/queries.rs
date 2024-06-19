pub fn list_overdue_query() -> &'static str {
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
    "
}

pub fn count_overdue_greater_query() -> &'static str {
    "
    MATCH 
        (a:Invoice)
    WHERE a.status = 'unpaid'
    WITH
        count(a) as count
    WHERE drasi.trueUntil(count > 2, date.transaction() + duration({days: 3}))
    RETURN
        count
    "
}
