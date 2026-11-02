MATCH (o:orders)
WHERE o.status = 'shipped'
RETURN o.id AS orderId, o.status AS status
