MATCH (o:recovery_orders)
WHERE o.needs_attention = true
RETURN o.episode_id AS episodeId,
       o.revision AS revision,
       o.reason AS reason
