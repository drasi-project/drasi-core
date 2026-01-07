-- Create database
IF NOT EXISTS (SELECT name FROM sys.databases WHERE name = 'OrdersDB')
BEGIN
    CREATE DATABASE OrdersDB;
END
GO

-- Enable snapshot isolation (must be done before USE)
ALTER DATABASE OrdersDB SET ALLOW_SNAPSHOT_ISOLATION ON;
GO

ALTER DATABASE OrdersDB SET READ_COMMITTED_SNAPSHOT ON;
GO

USE OrdersDB;
GO

-- Enable CDC on database (requires SQL Server Agent)
IF NOT EXISTS (SELECT * FROM sys.databases WHERE name = 'OrdersDB' AND is_cdc_enabled = 1)
BEGIN
    EXEC sys.sp_cdc_enable_db;
END
GO

-- Create Orders table
IF NOT EXISTS (SELECT * FROM sys.tables WHERE name = 'Orders' AND schema_id = SCHEMA_ID('dbo'))
BEGIN
    CREATE TABLE dbo.Orders (
        OrderId INT PRIMARY KEY IDENTITY(1,1),
        CustomerName NVARCHAR(100) NOT NULL,
        Status NVARCHAR(50) NOT NULL,
        TotalAmount DECIMAL(10,2) NOT NULL,
        CreatedAt DATETIME2 DEFAULT GETDATE()
    );
END
GO

-- Enable CDC on Orders table
IF NOT EXISTS (
    SELECT * FROM cdc.change_tables 
    WHERE source_object_id = OBJECT_ID('dbo.Orders')
)
BEGIN
    EXEC sys.sp_cdc_enable_table
        @source_schema = N'dbo',
        @source_name = N'Orders',
        @role_name = NULL,
        @supports_net_changes = 1;
END
GO

-- Insert sample data
TRUNCATE TABLE dbo.Orders;
GO

INSERT INTO dbo.Orders (CustomerName, Status, TotalAmount) VALUES
    ('Alice Johnson', 'Pending', 1250.00),
    ('Bob Smith', 'Completed', 750.50),
    ('Charlie Brown', 'Pending', 2100.00),
    ('Diana Prince', 'Shipped', 450.75),
    ('Eve Wilson', 'Pending', 890.25);
GO

SELECT 'Database setup complete!' AS Result;
SELECT * FROM dbo.Orders;
GO
