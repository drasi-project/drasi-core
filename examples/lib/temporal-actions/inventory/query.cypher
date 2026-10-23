MATCH (i:inventory)
WHERE i.stock < 5
RETURN i.product AS product, i.stock AS stock
