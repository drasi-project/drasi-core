use drasi_query_ast::ast;
use drasi_core::models::{QueryJoin, QueryJoinKey};

/**
apiVersion: query.reactive-graph.io/v1
kind: ContinuousQuery
metadata:
  name: crosses_above_a_threshold
spec:
  mode: query
  sources:
    subscriptions:
      - id: Reflex.CRM
        nodes:
          - sourceLabel: Customer
          - sourceLabel: Invoice
          - sourceLabel: InvoiceStatus
          - sourceLabel: Employee
          - sourceLabel: CustomerAccountManager
    joins:
      - id: HAS_INVOICE
        keys:
          - label: Customer
            property: id
          - label: Invoice
            property: cust_id
      - id: HAS_STATUS
        keys:
          - label: Invoice
            property: id
          - label: InvoiceStatus
            property: invoice_id
      - id: HAS_ACCOUNT_MANAGER
        keys:
          - label: Customer
            property: acc_mgr_id
          - label: Employee
            property: id
  query: > … Cypher Query …
*/

pub fn crosses_above_a_threshold_metadata() -> Vec<QueryJoin> {
    vec![
        QueryJoin {
            id: "HAS_INVOICE".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Customer".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "Invoice".into(),
                    property: "cust_id".into(),
                },
            ],
        },
        QueryJoin {
            id: "HAS_STATUS".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Invoice".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "InvoiceStatus".into(),
                    property: "invoice_id".into(),
                },
            ],
        },
        QueryJoin {
            id: "HAS_ACCOUNT_MANAGER".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Employee".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "Customer".into(),
                    property: "am_id".into(),
                },
            ],
        },
    ]
}

pub fn crosses_above_a_threshold_query() -> &'static str {
    "
  MATCH
    (cust:Customer)-[:HAS_INVOICE]->(invoice:Invoice)-[:HAS_STATUS]->(status:InvoiceStatus {status:'overdue'}),
    (cust:Customer)-[:HAS_ACCOUNT_MANAGER]->(employee:Employee)
  WITH
    employee.name AS accountManagerName, 
    employee.email AS accountManagerEmail,
    cust.name AS customerName,
    elementId(invoice) AS invoiceNumber,
    duration.inDays(date (invoice.due_date), datetime( {epochSeconds: status.timestamp } )) AS overdueDays
  WHERE
    overdueDays.days >= 10
  RETURN
    accountManagerName, accountManagerEmail, customerName, invoiceNumber"
}

pub fn crosses_above_a_threshold_with_overduedays_query() -> &'static str {
    "
  MATCH
    (cust:Customer)-[:HAS_INVOICE]->(invoice:Invoice)-[:HAS_STATUS]->(status:InvoiceStatus {status:'overdue'}),
    (cust:Customer)-[:HAS_ACCOUNT_MANAGER]->(employee:Employee)
  WITH
    employee.name AS accountManagerName, 
    employee.email AS accountManagerEmail,
    cust.name AS customerName,
    elementId(invoice) AS invoiceNumber,
    duration.inDays(date (invoice.due_date), datetime( {epochSeconds: status.timestamp } )) AS overdueDays
  WHERE
    overdueDays.days >= 10
  RETURN
    accountManagerName, accountManagerEmail, customerName, invoiceNumber, overdueDays"
}
