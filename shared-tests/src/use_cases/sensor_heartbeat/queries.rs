

pub fn not_reported_query() -> &'static str  {
            "
    MATCH
        (e:Equipment)-[:HAS_SENSOR]->(s:Sensor)-[:HAS_VALUE]->(v:SensorValue)
    WITH
        e,
        s,
        max(drasi.changeDateTime(v)) AS last_ts
    WHERE
        last_ts <= (datetime.realtime() - duration( { minutes: 60 } ))        
    OR
        drasi.trueLater(last_ts <= (datetime.realtime() - duration( { minutes: 60 } )), last_ts + duration( { minutes: 60 } ))
    RETURN
        e.name AS equipment,
        s.type AS sensor,
        last_ts AS last_ts
    "
}

pub fn percent_not_reported_query() -> &'static str  {
        "
    MATCH
        (e:Equipment)-[:HAS_SENSOR]->(s:Sensor)-[:HAS_VALUE]->(v:SensorValue)
    WITH
        e,
        max(drasi.changeDateTime(v)) AS last_ts
    WITH
        count(e) as total,
        count(
          CASE
            WHEN last_ts <= (datetime.realtime() - duration( { minutes: 60 } )) THEN 1
            WHEN drasi.trueLater(last_ts <= (datetime.realtime() - duration( { minutes: 60 } )), last_ts + duration( { minutes: 60 } )) THEN 1
            ELSE NULL
          END
        ) as not_reporting
    RETURN        
        (not_reporting / total) * 100 AS percent_not_reporting
    "
}
