MATCH (o:orders)
WHERE o.status = 'delivered'
RETURN o.id AS orderId, o.status AS status
