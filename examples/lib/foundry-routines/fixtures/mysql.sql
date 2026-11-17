CREATE USER 'drasi'@'%' IDENTIFIED BY 'local-fixture-only';
GRANT SELECT ON inventory.* TO 'drasi'@'%';
GRANT REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'drasi'@'%';
USE inventory;
CREATE TABLE inventory (
    sku VARCHAR(64) PRIMARY KEY,
    available INTEGER NOT NULL,
    description VARCHAR(255) NOT NULL DEFAULT ''
);
INSERT INTO inventory VALUES ('SKU-A', 10, ''), ('SKU-B', 10, '');
