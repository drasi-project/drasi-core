SET NOCOUNT ON;
CREATE TABLE {{schema}}.Items(id int NOT NULL PRIMARY KEY, value int NOT NULL, note nvarchar(100) NULL);
CREATE TABLE {{schema}}.Companion(id int NOT NULL PRIMARY KEY, value int NOT NULL);
CREATE TABLE {{schema}}.ReadOnlyItems(id int NOT NULL PRIMARY KEY, value int NOT NULL, note nvarchar(100) NULL);
CREATE TABLE {{schema}}.DrasiBarrier(token uniqueidentifier NOT NULL PRIMARY KEY);
CREATE TABLE {{schema}}.Effects(operation nvarchar(20), id bigint, value bigint, note nvarchar(max));
CREATE TABLE {{schema}}.Parameters(kind nvarchar(20), unsigned_value decimal(20,0), null_value int NULL,
  unicode_value nvarchar(max), json_value nvarchar(max), before_value nvarchar(max), after_value nvarchar(max));
CREATE TABLE {{schema}}.Scalars(flag bit, signed_value bigint, floating_value float, json_value nvarchar(max));
CREATE TABLE {{schema}}.Signals(name nvarchar(80) NOT NULL PRIMARY KEY);
EXEC sys.sp_cdc_enable_table @source_schema=N'{{schema_name}}',@source_name=N'Items',@role_name=NULL,@supports_net_changes=0;
EXEC sys.sp_cdc_enable_table @source_schema=N'{{schema_name}}',@source_name=N'Companion',@role_name=NULL,@supports_net_changes=0;
EXEC sys.sp_cdc_enable_table @source_schema=N'{{schema_name}}',@source_name=N'DrasiBarrier',@role_name=NULL,@supports_net_changes=0;
INSERT INTO {{schema}}.Items VALUES(1,10,N'雪'),(2,20,NULL);
INSERT INTO {{schema}}.Companion VALUES(1,10),(2,20);
INSERT INTO {{schema}}.ReadOnlyItems VALUES(1,10,N'雪'),(2,20,NULL);
GO
CREATE PROCEDURE {{schema}}.ApplyEffect @operation nvarchar(20), @id bigint, @value bigint
AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO {{schema}}.Effects(operation,id,value) VALUES(@operation,@id,@value);
  SELECT 1 AS first_result;
  SELECT 2 AS second_result;
END;
GO
CREATE PROCEDURE {{schema}}.BindParameters @kind nvarchar(20), @unsigned decimal(20,0), @nullable int,
  @unicode nvarchar(max), @json nvarchar(max), @before nvarchar(max), @after nvarchar(max)
AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO {{schema}}.Parameters VALUES(@kind,@unsigned,@nullable,@unicode,@json,@before,@after);
  SELECT @unsigned AS first_result;
  SELECT @unicode AS second_result;
  SELECT @json AS third_result;
END;
GO
CREATE PROCEDURE {{schema}}.BindScalars @flag bit, @signed bigint, @floating float, @json nvarchar(max)
AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO {{schema}}.Scalars VALUES(@flag,@signed,@floating,@json);
  SELECT @signed AS first_result;
  SELECT @floating AS second_result;
END;
GO
CREATE PROCEDURE {{schema}}.DelayedEffect @id bigint
AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO {{schema}}.Effects(operation,id,value) VALUES(N'ENTERED',@id,1);
  DECLARE @deadline datetime2=DATEADD(second,20,SYSUTCDATETIME());
  WHILE NOT EXISTS(SELECT 1 FROM {{schema}}.Signals WITH (READUNCOMMITTED) WHERE name=N'release-procedure')
  BEGIN
    IF SYSUTCDATETIME()>@deadline THROW 51000,'owned delayed procedure release deadline',1;
    WAITFOR DELAY '00:00:00.025';
  END;
  INSERT INTO {{schema}}.Effects(operation,id,value) VALUES(N'COMPLETED',@id,1);
  SELECT 1 AS completed;
END;
GO
CREATE PROCEDURE {{schema}}.PartialFailure @id bigint
AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO {{schema}}.Effects(operation,id,value) VALUES(N'BEFORE-ERROR',@id,1);
  SELECT 1 AS partial_result;
  SELECT 2 AS another_partial_result;
  THROW 51001,'intentional owned procedure error after an autocommitted effect',1;
END;
GO
GRANT SELECT, VIEW DEFINITION ON SCHEMA::{{schema}} TO {{app_user}};
GRANT SELECT ON OBJECT::cdc.[{{schema_name}}_Items_CT] TO {{app_user}};
GRANT SELECT ON OBJECT::cdc.[{{schema_name}}_Companion_CT] TO {{app_user}};
GRANT SELECT ON OBJECT::cdc.[{{schema_name}}_DrasiBarrier_CT] TO {{app_user}};
GRANT SELECT ON OBJECT::cdc.[fn_cdc_get_all_changes_{{schema_name}}_Items] TO {{app_user}};
GRANT SELECT ON OBJECT::cdc.[fn_cdc_get_all_changes_{{schema_name}}_Companion] TO {{app_user}};
GRANT SELECT ON OBJECT::cdc.[fn_cdc_get_all_changes_{{schema_name}}_DrasiBarrier] TO {{app_user}};
GRANT INSERT,DELETE ON {{schema}}.DrasiBarrier TO {{app_user}};
GRANT EXECUTE ON SCHEMA::{{schema}} TO {{app_user}};
GRANT SELECT, VIEW DEFINITION ON {{schema}}.ReadOnlyItems TO {{reader_user}};
GO
