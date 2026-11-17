MATCH (o:orders)-[:STOCK_FOR]->(i:inventory)
WHERE o.status = 'pending' AND i.available < o.quantity
RETURN o.order_id AS order_id,
       o.sku AS sku,
       o.quantity AS required,
       i.available AS available
