// Copyright 2024 The Drasi Authors.
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

pub fn manager_incident_alert_query() -> &'static str {
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

pub fn employee_incident_alert_query() -> &'static str {
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

pub fn employees_at_risk_count_query() -> &'static str {
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
