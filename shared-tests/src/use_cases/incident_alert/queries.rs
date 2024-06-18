use drasi_query_ast::ast;

pub fn manager_incident_alert_query() -> &'static str  {
    "
  MATCH
    (e:Employee)-[:ASSIGNED_TO]->(t:Team),
    (m:Employee)-[:MANAGES]->(t:Team),
    (e:Employee)-[:LOCATED_IN]->(:Building)-[:LOCATED_IN]->(r:Region),
    (i:Incident {type:'environmental'})-[:OCCURS_IN]->(r:Region)
  WHERE
    elementId(e) <> elementId(m) AND i.severity IN ['critical','extreme'] AND i.endTimeMs IS NULL
  RETURN
    m.name AS ManagerName, m.email AS ManagerEmail,
    e.name AS EmployeeName, e.email AS EmployeeEmail,
    r.name AS RegionName,
    elementId(i)  AS IncidentId, i.severity AS IncidentSeverity, i.description AS IncidentDescription
    "
}

pub fn employee_incident_alert_query() -> &'static str  {
    "
  MATCH
    (e:Employee)-[:LOCATED_IN]->(:Building)-[:LOCATED_IN]->(r:Region),
    (i:Incident)-[:OCCURS_IN]->(r:Region)
  WHERE
    i.severity IN ['critical','extreme'] AND i.endTimeMs IS NULL
  RETURN
    e.name AS EmployeeName, e.email AS EmployeeEmail,
    r.name AS RegionName,
    elementId(i)  AS IncidentId, i.severity AS IncidentSeverity, i.description AS IncidentDescription
    "
}

pub fn employees_at_risk_count_query() -> &'static str  {
    "
  MATCH
    (e:Employee)-[:LOCATED_IN]->(:Building)-[:LOCATED_IN]->(r:Region),
    (i:Incident)-[:OCCURS_IN]->(r:Region)
  WHERE
    i.severity IN ['critical','extreme'] AND i.endTimeMs IS NULL
  RETURN
    r.name AS RegionName,
    elementId(i)  AS IncidentId, i.severity AS IncidentSeverity, i.description AS IncidentDescription,
    count(e.name) AS EmployeeCount
    "
}
