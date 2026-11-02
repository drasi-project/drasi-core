MATCH (o:orders)
WHERE o.status = 'placed'
RETURN o.id AS orderId, o.status AS status
