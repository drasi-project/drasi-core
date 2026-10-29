SET NOCOUNT ON;
DECLARE @schema sysname=N'{{schema_name}}', @table sysname, @capture sysname;
IF (SELECT COUNT(*) FROM {{schema}}.__DrasiOwned WHERE token=N'{{owned_token}}')<>1
  THROW 51003,'owned schema token changed; refusing cleanup',1;
IF EXISTS(SELECT 1 FROM sys.objects WHERE schema_id=SCHEMA_ID(@schema)
  AND type IN ('U','P','V','FN','IF','TF','TR','SO')
  AND name NOT IN ('__DrasiOwned','Items','Companion','ReadOnlyItems','DrasiBarrier','Effects','Parameters','Scalars','Signals',
    'ApplyEffect','BindParameters','BindScalars','DelayedEffect','PartialFailure'))
  THROW 51002,'unexpected object in owned SQL Server test schema; manual review required',1;
DECLARE captures CURSOR LOCAL FAST_FORWARD FOR
  SELECT t.name,ct.capture_instance FROM cdc.change_tables ct
  JOIN sys.tables t ON t.object_id=ct.source_object_id
  WHERE t.schema_id=SCHEMA_ID(@schema) AND t.name IN ('Items','Companion','DrasiBarrier');
OPEN captures;
FETCH NEXT FROM captures INTO @table,@capture;
WHILE @@FETCH_STATUS=0
BEGIN
  EXEC sys.sp_cdc_disable_table @source_schema=@schema,@source_name=@table,@capture_instance=@capture;
  FETCH NEXT FROM captures INTO @table,@capture;
END;
CLOSE captures;
DEALLOCATE captures;
DROP PROCEDURE IF EXISTS {{schema}}.ApplyEffect, {{schema}}.BindParameters, {{schema}}.BindScalars, {{schema}}.DelayedEffect, {{schema}}.PartialFailure;
DROP TABLE IF EXISTS {{schema}}.Items, {{schema}}.Companion, {{schema}}.ReadOnlyItems, {{schema}}.DrasiBarrier,
  {{schema}}.Effects, {{schema}}.Parameters, {{schema}}.Scalars, {{schema}}.Signals;
-- Refuse to remove a namespace into which anything unexpected was added.
IF EXISTS(SELECT 1 FROM sys.objects WHERE schema_id=SCHEMA_ID(@schema) AND name<>N'__DrasiOwned')
  THROW 51002,'unexpected object in owned SQL Server test schema; manual review required',1;
DROP TABLE {{schema}}.__DrasiOwned;
EXEC(N'DROP SCHEMA {{schema}}');
