MATCH (o:orders)
WHERE o.status = 'paid'
RETURN o.id AS orderId, o.status AS status
